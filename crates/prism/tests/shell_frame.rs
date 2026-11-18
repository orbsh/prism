//! The shell arrives before any application speaks (ADR-0019).
//!
//! Two facts, and the order between them is the point:
//!
//! 1. the page's frame is the VENUE's speech — it is drawn from a file the operator writes, so it
//!    exists whether or not an application answered;
//! 2. the frame comes BEFORE `ui:connect`, because a booth that answers that event with a `set`
//!    needs its region to exist. A frame that arrived after the application's first frame would
//!    erase it — `create` replaces the root.
//!
//! What this file does NOT claim: that a real booth fills its region. That pairing is locked where
//! each side lives (gravity's own test asserts it `set`s its island slot when the payload says
//! `shell: true`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use aura_engine::Engine;
use prism::{
    identity::Registry,
    nodes::Posture,
    shell::Shell,
    Gateway,
};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as Ws;
use tokio_tungstenite::WebSocketStream;

type WsClient = WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

const FRAME: &str = r#"{
  "type": "case",
  "children": [
    {"type": "placeholder", "bind": {"value": {"kind": "source", "source": "auth"}}},
    {"type": "case", "bind": {"value": {"kind": "source", "source": "island:gravity_turn"}}}
  ]
}"#;

async fn boot(frame: &str) -> std::net::SocketAddr {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("shell.json");
    std::fs::write(&file, frame).unwrap();
    let registry = Registry::open(&dir.keep().join("data")).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::with_deployment(
        engine,
        registry,
        Posture::Open,
        prism::Deployment {
            assets: None,
            shell: Some(Shell::open(&file).unwrap()),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = gw.serve(listener).await;
    });
    addr
}

async fn recv_json(ws: &mut WsClient) -> Value {
    loop {
        match ws.next().await.expect("stream").expect("ws msg") {
            Ws::Binary(b) => return serde_json::from_slice(&b).expect("json frame"),
            Ws::Text(t) => return serde_json::from_str(&t).expect("json frame"),
            Ws::Close(_) => panic!("server closed"),
            _ => continue,
        }
    }
}

use futures_util::StreamExt;

#[tokio::test]
async fn the_frame_is_drawn_from_the_file_before_the_application_is_asked() {
    let addr = boot(FRAME).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/channel?codec=json"))
        .await
        .expect("/channel upgrades");

    // The first things a page hears are the venue's: the credential (an inject), then the frame (a
    // create). Reading until the create is what makes this order-independent without hiding it —
    // the assertion below fails if it never comes.
    let mut created = None;
    for _ in 0..4 {
        let frame = recv_json(&mut ws).await;
        let Some(ops) = frame.as_array() else { continue };
        if let Some(op) = ops
            .iter()
            .find(|op| op.get("action").and_then(Value::as_str) == Some("create"))
        {
            created = Some(op.clone());
            break;
        }
    }
    let create = created.expect("the frame is created on connect");

    // The file travels unchanged: the venue wraps it in a `create`, it does not rewrite it.
    let children = create["data"]["children"].as_array().expect("a case has children");
    assert_eq!(children.len(), 2, "{}", create["data"]);
    assert_eq!(
        children[1]["bind"]["value"]["source"], "island:gravity_turn",
        "the region an island draws into is declared by the FILE, not by the venue"
    );
}