//! The renderer's leg e2e (PLAN Phase 1.10): `/assets` static face + `init` on `/ui`.
//!
//! Locks what makes the browser leg real: the bundle is served as plain downloads on the
//! shared accept loop; `/ui` speaks fluxen's wire (`Message` down, `Outflow` up) instead of
//! the peer leg's `{ev, args}`; `init` delivers the layout as ORDINARY view frames — no
//! layout envelope, no second frame shape — and a deployment with no `PRISM_INIT` still
//! answers (`init.error`), it does not hang.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use aura_engine::Engine;
use futures_util::{SinkExt, StreamExt};
use prism::{assets::AssetRoot, identity::Registry, init::InitSource, Deployment, Gateway};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as Ws;

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn boot(deployment: Deployment) -> SocketAddr {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.into_path()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::with_deployment(engine, registry, prism::nodes::Posture::Open, deployment);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });
    addr
}

/// One raw HTTP GET, full response (the route closes the connection).
async fn get(addr: SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

/// One raw HTTP POST with a JSON body (the admin faces).
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

/// A renderer connection: the browser leg, JSON pinned for readable assertions.
async fn open_ui(addr: SocketAddr) -> WsClient {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ui?codec=json"))
        .await
        .expect("/ui upgrades");
    ws
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

async fn send_uplink(ws: &mut WsClient, value: Value) {
    ws.send(Ws::Text(serde_json::to_string(&value).unwrap().into()))
        .await
        .unwrap();
}

/// The layout document an application ships: a list of Accrete frames.
fn layout_file() -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        f.path(),
        "- action: create\n  event: \"\"\n  data: {type: case, attrs: {}}\n\
         - action: set\n  event: chat\n  data: {type: case}\n",
    )
    .unwrap();
    f
}

#[tokio::test]
async fn assets_are_plain_downloads_on_the_shared_loop() {
    let bundle = tempfile::tempdir().unwrap();
    std::fs::write(bundle.path().join("index.html"), "<!doctype html><div id=main></div>").unwrap();
    std::fs::write(bundle.path().join("ui_leptos-abc123.js"), "console.log(1)").unwrap();
    let addr = boot(Deployment {
        assets: Some(AssetRoot::open(bundle.path()).unwrap()),
        init: None,
    })
    .await;

    // Hashed asset: content-typed and immutable for a year.
    let resp = get(addr, "/assets/ui_leptos-abc123.js").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(resp.contains("Content-Type: text/javascript; charset=utf-8"), "{resp}");
    assert!(resp.contains("immutable"), "{resp}");
    assert!(resp.ends_with("console.log(1)"), "{resp}");

    // The mount root is the entry document, revalidated (not immutable).
    let resp = get(addr, "/assets/").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(resp.contains("Content-Type: text/html; charset=utf-8"), "{resp}");
    assert!(resp.contains("no-cache"), "{resp}");

    // An escape and a miss are the same answer: 404, no listing, no partial path.
    for path in ["/assets/../Cargo.toml", "/assets/nope.js"] {
        let resp = get(addr, path).await;
        assert!(resp.starts_with("HTTP/1.1 404"), "{path} → {resp}");
    }
}

#[tokio::test]
async fn init_delivers_the_layout_as_view_frames() {
    let layout = layout_file();
    let spec = format!("file:{}", layout.path().display());
    let addr = boot(Deployment {
        assets: None,
        init: Some(InitSource::parse(&spec).expect("file source")),
    })
    .await;

    let mut ws = open_ui(addr).await;
    // The connect answer rides the renderer's own shape: `ev != "draw"`, so a renderer
    // ignores it and a client that cares can read the sender.
    let hello = recv_json(&mut ws).await;
    assert_eq!(hello["ev"], "connected");
    assert!(hello["sender"].as_str().unwrap().starts_with("device:"));
    let empty = &hello["content"];
    assert!(empty.is_null() || empty.as_array().is_some_and(|a| a.is_empty()), "{hello}");

    // `init` is an ordinary uplink; its answer is the layout, frame by frame.
    send_uplink(&mut ws, json!({"event": "init", "data": null})).await;
    // `Message.content` is serialized one-or-many: a single operation IS the object, not a
    // one-element array. That is the renderer's wire, not a prism choice.
    let first = recv_json(&mut ws).await;
    assert_eq!(first["ev"], "draw");
    assert_eq!(first["content"]["action"], "create");
    assert_eq!(first["content"]["data"]["type"], "case");

    let second = recv_json(&mut ws).await;
    assert_eq!(second["ev"], "draw");
    assert_eq!(second["content"]["action"], "set");
    assert_eq!(second["content"]["event"], "chat");

    // The terminator carries no payload: the layout IS the frames above.
    let done = recv_json(&mut ws).await;
    assert_eq!(done["ev"], "init.result");
    let empty = &done["content"];
    assert!(empty.is_null() || empty.as_array().is_some_and(|a| a.is_empty()), "{done}");
}

#[tokio::test]
async fn a_deployment_without_init_answers_instead_of_hanging() {
    let addr = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let _hello = recv_json(&mut ws).await;
    send_uplink(&mut ws, json!({"event": "init", "data": null})).await;
    let refused = recv_json(&mut ws).await;
    assert_eq!(refused["ev"], "init.error", "{refused}");
    // The socket stays up: the next uplink still gets an answer.
    send_uplink(&mut ws, json!({"event": "init", "data": null})).await;
    assert_eq!(recv_json(&mut ws).await["ev"], "init.error");
}

/// Two legs, two shapes: `/ws` keeps `{ev, args}` while `/ui` speaks the renderer's wire.
#[tokio::test]
async fn the_peer_leg_keeps_its_own_shape() {
    let addr = boot(Deployment::default()).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?protocol=json"))
        .await
        .expect("/ws upgrades");
    let frame: Value = loop {
        match ws.next().await.expect("stream").expect("ws msg") {
            Ws::Binary(b) => break serde_json::from_slice(&b).unwrap(),
            Ws::Text(t) => break serde_json::from_str(&t).unwrap(),
            Ws::Close(_) => panic!("closed"),
            _ => continue,
        }
    };
    assert_eq!(frame["ev"], "connected");
    assert!(frame.get("args").is_some(), "peer leg answers with args: {frame}");
    assert!(frame.get("content").is_none(), "peer leg carries no Accrete: {frame}");
}

/// Application uplinks reach the booth plane: the booth NAMED BY THE EVENT is called, and
/// its answer comes back as frames (a terminator at minimum). `echo_steel` answers with the
/// envelope, which is not an Accrete operation — the leg skips it, logs it, and still
/// terminates the answer. The point under test is the DISPATCH, not the echo's shape.
#[tokio::test]
async fn an_uplink_dispatches_to_the_booth_named_by_the_event() {
    let addr = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let _hello = recv_json(&mut ws).await;
    send_uplink(&mut ws, json!({"event": "echo_steel", "data": {"msg": "hi"}})).await;
    let done = recv_json(&mut ws).await;
    assert_eq!(done["ev"], "echo_steel.result", "{done}");
}

/// The per-event auth declaration is enforced on THIS leg too, with the same surface: a
/// booth that declares `auth` refuses an anonymous renderer, and the socket stays up.
#[tokio::test]
async fn the_auth_declaration_gates_the_renderer_leg() {
    let addr = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let _hello = recv_json(&mut ws).await;

    send_uplink(&mut ws, json!({"event": "echo_priv", "data": {}})).await;
    assert_eq!(recv_json(&mut ws).await["ev"], "error", "anonymous sender must be refused");

    // signup on the leg: the account is prism's, and the answer carries who we are.
    send_uplink(
        &mut ws,
        json!({"event": "signup", "data": {"username": "ada", "password": "correct horse"}}),
    )
    .await;
    let signed = recv_json(&mut ws).await;
    assert_eq!(signed["ev"], "signup.result", "{signed}");
    assert!(signed["sender"].as_str().unwrap().starts_with("user:"), "{signed}");

    // Bound now: the declared-auth event reaches the booth (its answer terminates).
    send_uplink(&mut ws, json!({"event": "echo_priv", "data": {}})).await;
    assert_eq!(recv_json(&mut ws).await["ev"], "echo_priv.result");

    // After logout the same event is refused again — the field, not the account row, is
    // what the gate reads, and it is cleared on the same socket.
    send_uplink(&mut ws, json!({"event": "logout", "data": null})).await;
    assert!(recv_json(&mut ws).await["sender"].as_str().unwrap().starts_with("device:"));
    send_uplink(&mut ws, json!({"event": "echo_priv", "data": {}})).await;
    assert_eq!(recv_json(&mut ws).await["ev"], "error");
}

/// Revocation reaches BOTH legs: a revoked device's renderer socket is closed, not left
/// watching a page that still believes it is logged in.
#[tokio::test]
async fn revocation_kicks_the_renderer_leg_too() {
    let addr = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let hello = recv_json(&mut ws).await;
    let device: u64 = hello["sender"]
        .as_str()
        .unwrap()
        .strip_prefix("device:")
        .unwrap()
        .parse()
        .unwrap();

    // Bind an account so this connection carries an auth field worth clearing.
    send_uplink(&mut ws, json!({"event": "signup", "data": {"username": "grace", "password": "p"}})).await;
    assert_eq!(recv_json(&mut ws).await["ev"], "signup.result");

    let resp = post_json(addr, "/admin/revoke", json!({"device_id": device})).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");

    // The pump closes the socket (the table dropped its sender).
    let mut closed = false;
    while let Some(msg) = ws.next().await {
        if matches!(msg.expect("ws msg"), Ws::Close(_)) {
            closed = true;
            break;
        }
    }
    assert!(closed, "a revoked renderer connection must be closed");
}
