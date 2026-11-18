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
use futures_util::SinkExt;
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
  "type": "frame",
  "regions": [{"booth": "gravity-model", "label": "对话"}],
  "children": [
    {"type": "frame", "bind": {"value": {"kind": "source", "source": "auth"}}},
    {"type": "pages", "bind": {
      "value": {"kind": "source", "source": "pages"},
      "select": {"kind": "local", "slot": "page", "path": "/data", "default": "gravity-model"}
    }}
  ]
}"#;

async fn boot(frame: &str) -> (std::net::SocketAddr, std::sync::Arc<Gateway>) {
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
    let serving = gw.clone();
    tokio::spawn(async move {
        let _ = serving.serve(listener).await;
    });
    (addr, gw)
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
    let (addr, _gw) = boot(FRAME).await;
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
        create["data"]["regions"][0]["booth"], "gravity-model",
        "the region is declared by the FILE, and the declaration renders nothing"
    );
    assert_eq!(
        children[1]["type"], "pages",
        "the body is the pager: rows come from a list slot, never from static children"
    );
}

/// The other half of the convention (ADR-0019): an island writes its BARE names and the venue puts
/// them in its namespace. The booth never spells `island:…` — the prefix is the venue's, and the
/// reason it is the venue's is that a booth spelling it would have to be told a name it should not
/// need, and could then spell someone else's.
#[tokio::test]
async fn an_islands_slots_arrive_namespaced() {
    let (addr, gw) = boot(FRAME).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/channel?codec=json"))
        .await
        .expect("/channel upgrades");

    // No address: a broadcast reaches every live renderer. The booth's own uplink would name a
    // device; the venue's rewriting is the same either way, which is what this test is about.
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "ui:gravity-model.layout",
        serde_json::json!({
            "name": "ui:gravity-model.layout",
            // The booth says which region this surface belongs to; the event name cannot.
            "island": "gravity-model",
            "data": [{"action": "set", "slot": "pages", "data": {"type": "frame"}}],
        }),
    )
    .await
    .expect("emit");

    let mut landed = None;
    for _ in 0..40 {
        let frame = recv_json(&mut ws).await;
        let Some(ops) = frame.as_array() else { continue };
        // The venue's own frame data also arrives as `set` (the `user` slot), so this looks for the
        // one thing under test: an application frame that has been moved into a namespace.
        if let Some(op) = ops.iter().find(|op| {
            op.get("action").and_then(Value::as_str) == Some("set")
                && op.get("slot")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.starts_with("island:"))
        }) {
            landed = Some(op.clone());
            break;
        }
    }
    let op = landed.expect("the application's frame reached the page");
    assert_eq!(
        op["slot"], "island:gravity-model/pages",
        "the page's name for the surface namespaces every slot the booth writes: {op}"
    );
}

/// WHERE THE PAGE OPENS, and how it moves: `ui:connect` names the island (the shell's FIRST unless
/// the account chose one), and walking to another island is answered with the same `connect` — so a
/// booth draws lazily, when the person is looking at it, and says nothing otherwise.
#[tokio::test]
async fn a_page_opens_on_the_first_island_and_moves_on_request() {
    const TWO: &str = r#"{
      "type": "frame",
      "children": [
        {"type": "frame", "bind": {"value": {"kind": "source", "source": "island:alpha"}}},
        {"type": "frame", "bind": {"value": {"kind": "source", "source": "island:beta"}}}
      ]
    }"#;
    let (addr, _gw) = boot(TWO).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/channel?codec=json"))
        .await
        .expect("/channel upgrades");

    let mut opened = None;
    for _ in 0..6 {
        let frame = recv_json(&mut ws).await;
        let Some(ops) = frame.as_array() else { continue };
        if let Some(op) = ops
            .iter()
            .find(|op| op.get("action").and_then(Value::as_str) == Some("inject"))
        {
            if op.get("name").and_then(Value::as_str) == Some("connect") {
                opened = Some(op["data"].clone());
                break;
            }
        }
    }
    let opened = opened.expect("the page is told which island it opens on");
    assert_eq!(opened["island"].as_str(), Some("alpha"), "{opened}");
    assert_eq!(
        opened["islands"],
        serde_json::json!(["alpha", "beta"]),
        "the frame's islands, in document order: {opened}"
    );

    // Walking to the second island is answered with the same fact, naming the new one — in the
    // shape a renderer actually reports it: a `select` whose binding carries `report` sends the
    // VALUE ITSELF (fluxen's `BindVariant::Local.report`, ADR-0028), not an object with a field
    // named after our intent.
    ws.send(Ws::Text(
        serde_json::to_string(&serde_json::json!({
            "dest": "island",
            "data": "beta"
        }))
        .unwrap()
        .into(),
    ))
    .await
    .expect("the uplink goes out");

    let mut moved = None;
    for _ in 0..6 {
        let frame = recv_json(&mut ws).await;
        let Some(ops) = frame.as_array() else { continue };
        if let Some(op) = ops
            .iter()
            .find(|op| op.get("action").and_then(Value::as_str) == Some("inject"))
        {
            if op.get("name").and_then(Value::as_str) == Some("connect") {
                moved = Some(op["data"].clone());
                break;
            }
        }
    }
    let moved = moved.expect("the walk is answered");
    assert_eq!(moved["island"].as_str(), Some("beta"), "{moved}");
}


/// The frame's two dynamic slots, filled by the venue (ADR-0019): who is looking, and what the
/// navigation offers. Both are facts a booth is never told, and both are needed BEFORE any
/// application answers — a bar that waited for an application would be empty while it booted, and
/// empty forever if it failed.
#[tokio::test]
async fn the_frame_gets_its_viewer_and_its_navigation() {
    let (addr, _gw) = boot(FRAME).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/channel?codec=json"))
        .await
        .expect("/channel upgrades");

    let mut user = None;
    let mut nav = Vec::new();
    let mut pages = Vec::new();
    for _ in 0..10 {
        let frame = recv_json(&mut ws).await;
        let Some(ops) = frame.as_array() else { continue };
        for op in ops {
            match op["action"].as_str() {
                Some("set") if op["slot"] == "user" => user = Some(op.clone()),
                Some("append") if op["slot"] == "nav" => nav.push(op.clone()),
                Some("append") if op["slot"] == "pages" => pages.push(op.clone()),
                _ => {}
            }
        }
        if user.is_some() && !nav.is_empty() && !pages.is_empty() {
            break;
        }
    }

    let user = user.expect("the frame's viewer arrives on connect");
    let shown = user["data"]["bind"]["value"]["default"]
        .as_str()
        .expect("a text node carries the name as its default");
    // A fresh connection is an unbound device: the header says so rather than nothing, because
    // "nobody yet" is an answer and an empty header is not.
    assert!(shown.contains("guest"), "{user}");

    assert_eq!(nav.len(), 1, "one item per island the frame declares: {nav:?}");
    // The pager's row: the same id as the navigation item (so a click can only name a page that
    // exists), holding the binding that shows the region.
    assert_eq!(pages.len(), 1, "one page per island: {pages:?}");
    assert_eq!(pages[0]["data"]["id"], "gravity-model");
    assert_eq!(
        pages[0]["data"]["bind"]["value"]["source"], "island:gravity-model",
        "the row shows the region — the frame declared it, the venue supplied the row"
    );
    assert_eq!(
        pages[0]["data"]["type"], "frame",
        "a FRAME bound to a source renders the node tree that slot holds (fluxen ADR 0027: the old \
         static-only `case` is gone, and a row that only carried a name drew nothing)"
    );
    assert_eq!(
        nav[0]["data"]["bind"]["value"]["default"], "对话",
        "the label comes from the FRAME (its attrs.label), not from a table the venue keeps"
    );
}
