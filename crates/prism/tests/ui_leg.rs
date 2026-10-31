//! The renderer's leg, end to end: `/channel` speaks fluxen's wire — DOWNSTREAM AN ACTION
//! LIST (no envelope, fluxen 460d867), UPSTREAM `Outflow{event, id?, data}` — and everything
//! the VENUE has to say rides the same wire as an `inject` (fluxen ADR 0011), because a
//! renderer refuses a map outright: "frame is an envelope, not an action list".
//!
//! That refusal is why these tests never assert `frame["ev"]`: the leg has no verbs any more.
//! The venue's speech is `inject` into a slot named after the fact it states — `identity` (who am
//! I), `connect` (a connection opened, which the application answers with its layout) or `notice`
//! (what failed); everything else is the application's own operations.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use aura_engine::Engine;
use futures_util::{SinkExt, StreamExt};
use prism::{assets::AssetRoot, identity::Registry, Deployment, Gateway};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as Ws;

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn boot(deployment: Deployment) -> (SocketAddr, std::sync::Arc<Gateway>) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::with_deployment(engine, registry, prism::nodes::Posture::Open, deployment);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = std::sync::Arc::clone(&gw);
    tokio::spawn(async move { served.serve(listener).await.unwrap() });
    (addr, gw)
}

/// The two facts the venue says to a connection that just opened: `identity` (who this is, and the
/// token the page must keep) and `connect` (the fact an application answers with its layout). Both
/// are EVENTS on the plane like any other, so the page sees a slot named after the fact — there is
/// no per-verb `connected`/`signup.result` name to learn. They are two emits, and the plane promises
/// no order between them: a page binds both, and neither assumes the other arrived first.
async fn read_connected(ws: &mut WsClient) -> String {
    let mut identity_device: Option<String> = None;
    let mut connect_device: Option<String> = None;
    for _ in 0..2 {
        let (slot, name, data) = recv_speech(ws).await;
        assert_eq!(slot, name, "the slot is named after the fact: {name}");
        match name.as_str() {
            // The device id is a uuid: TEXT on this wire (a 128-bit id is not a JSON number), and
            // the same fact also carries the token the page must keep.
            "identity" => {
                assert!(data["token"].as_str().is_some_and(|t| t.len() == 36), "{data}");
                identity_device =
                    Some(data["device"].as_str().expect("device id in identity").to_string());
            }
            "connect" => {
                connect_device =
                    Some(data["device"].as_str().expect("the fact names the device").to_string());
            }
            other => panic!("unexpected venue speech on connect: `{other}`"),
        }
    }
    let device = identity_device.expect("the venue says who this connection is");
    assert_eq!(
        connect_device.as_deref(),
        Some(device.as_str()),
        "the connect fact names this device"
    );
    device
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
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/channel?codec=json"))
        .await
        .expect("/channel upgrades");
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

/// One message from this leg is ONE ACTION LIST. A non-array is the HOST's bug (the renderer
/// would refuse it by name), so the test fails on the shape instead of indexing a map.
async fn recv_ops(ws: &mut WsClient) -> Vec<Value> {
    let frame = recv_json(ws).await;
    frame
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("not an action list: {frame}"))
}

/// The venue's own speech: one injected event into `identity` or `notice`.
async fn recv_speech(ws: &mut WsClient) -> (String, String, Value) {
    let ops = recv_ops(ws).await;
    assert_eq!(ops.len(), 1, "venue speech is one operation: {ops:?}");
    let op = &ops[0];
    assert_eq!(op["action"], "inject", "expected venue speech: {op}");
    (
        op["slot"].as_str().unwrap().to_string(),
        op["name"].as_str().unwrap().to_string(),
        op["data"].clone(),
    )
}

async fn send_uplink(ws: &mut WsClient, value: Value) {
    ws.send(Ws::Text(serde_json::to_string(&value).unwrap().into()))
        .await
        .unwrap();
}

/// The layout document an application ships: a list of Accrete frames.
#[tokio::test]
async fn assets_are_plain_downloads_on_the_shared_loop() {
    let bundle = tempfile::tempdir().unwrap();
    std::fs::write(bundle.path().join("index.html"), "<!doctype html><div id=main></div>").unwrap();
    std::fs::write(bundle.path().join("ui_leptos-abc123.js"), "console.log(1)").unwrap();
    let (addr, _gw) = boot(Deployment { assets: Some(AssetRoot::open(bundle.path()).unwrap()) })
    .await;

    // Hashed asset: content-typed and immutable for a year.
    let resp = get(addr, "/ui_leptos-abc123.js").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(resp.contains("Content-Type: text/javascript; charset=utf-8"), "{resp}");
    assert!(resp.contains("immutable"), "{resp}");
    assert!(resp.ends_with("console.log(1)"), "{resp}");

    // The mount root is the entry document, revalidated (not immutable).
    let resp = get(addr, "/").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(resp.contains("Content-Type: text/html; charset=utf-8"), "{resp}");
    assert!(resp.contains("no-cache"), "{resp}");

    // An escape and a miss are the same answer: 404, no listing, no partial path.
    for path in ["/../Cargo.toml", "/nope.js"] {
        let resp = get(addr, path).await;
        assert!(resp.starts_with("HTTP/1.1 404"), "{path} → {resp}");
    }
}

/// The connection is a FACT on the plane, not a configured layout: the venue emits
/// `ui:connect {device}` and whoever declared it answers with frames. This is what lets one
/// application be assembled from several booths — and why `PRISM_INIT` is gone.
#[tokio::test]
async fn connect_is_an_event_and_the_application_answers_it() {
    let (addr, gw) = boot(Deployment::default()).await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let collector = std::sync::Arc::clone(&seen);
    gw.engine
        .register(aura_booth::BoothType {
            name: "connect_probe".into(),
            body: aura_booth::Body::Rust(std::sync::Arc::new(move |_ctx, args| {
                let collector = std::sync::Arc::clone(&collector);
                Box::pin(async move {
                    collector.lock().unwrap().push(args);
                    Ok(Value::Null)
                })
            })),
            idle_ttl: None,
            max_exec: None,
            hooks: std::collections::BTreeMap::new(),
            receives: vec![aura_booth::ReceiveDecl {
                event: "ui:connect".into(),
                entry: None,
                resolution: aura_booth::RouteResolution::Singleton,
                wildcard: false,
            }],
        })
        .await
        .expect("register the connect probe");

    let mut ws = open_ui(addr).await;
    let device = read_connected(&mut ws).await;

    // Delivery is queued on the plane, so wait for it rather than race it.
    let mut fact = None;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if let Some(first) = seen.lock().unwrap().first() {
            fact = Some(first.clone());
            break;
        }
    }
    let fact = fact.expect("ui:connect reached the plane");
    assert_eq!(fact["device"].as_str(), Some(device.as_str()), "the fact names the device: {fact}");

    // The application's answer: frames the venue forwards AS the message (one action list).
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "ui:layout",
        json!({"name": "ui:layout", "device": device, "data": [
            {"action": "create", "slot": "", "data": {"type": "case", "attrs": {"class": ["from-event"]}}},
            {"action": "set", "slot": "graph", "data": {"type": "case"}},
        ]}),
    )
    .await
    .expect("emit ui:layout");

    let ops = recv_ops(&mut ws).await;
    assert_eq!(ops.len(), 2, "both frames in one list: {ops:?}");
    assert_eq!(ops[0]["action"], "create");
    assert_eq!(ops[0]["data"]["attrs"]["class"][0], "from-event");
    assert_eq!(ops[1]["slot"], "graph");
}

/// Two legs, two shapes: `/ws` keeps `{ev, args}` while `/channel` speaks the renderer's wire.
#[tokio::test]
async fn the_peer_leg_keeps_its_own_shape() {
    let (addr, _gw) = boot(Deployment::default()).await;
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

/// Application uplinks reach the booth plane: the booth NAMED BY THE EVENT is called, and it is
/// called with the venue's envelope — `{sender: {device, user}, args: <uplink data>}`. Identity
/// rides the payload (ADR-0017 §7), never the instance id, so this is the only place a booth can
/// learn who asked; `echo_steel` hands that envelope straight back, which makes it readable here.
#[tokio::test]
async fn an_uplink_dispatches_to_the_booth_named_by_the_event() {
    let (addr, _gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    read_connected(&mut ws).await;
    // The booth's answer IS the operation the caller sent: an answer that is not an operation
    // is data, and data has no path down this leg — a booth that wants the page to see
    // something emits `ui:<name>`. So the drawn frame here proves the dispatch happened.
    let op = json!({"action": "set", "slot": "graph", "data": {"type": "case", "attrs": {"class": ["echoed"]}}});
    send_uplink(&mut ws, json!({"dest": "echo_frame", "data": op})).await;
    let ops = recv_ops(&mut ws).await;
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0]["action"], "set");
    assert_eq!(ops[0]["slot"], "graph");
    assert_eq!(ops[0]["data"]["attrs"]["class"][0], "echoed");
}

/// The per-event auth declaration is enforced on THIS leg too, with the same surface: a booth
/// that declares `auth` refuses an anonymous renderer with a `notice`, and the socket stays up.
#[tokio::test]
async fn the_auth_declaration_gates_the_renderer_leg() {
    let (addr, _gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    read_connected(&mut ws).await;

    let op = json!({"action": "set", "slot": "secret", "data": {"type": "text"}});
    send_uplink(&mut ws, json!({"dest": "echo_gate", "data": op})).await;
    let (slot, name, data) = recv_speech(&mut ws).await;
    assert_eq!((slot.as_str(), name.as_str()), ("notice", "notice"), "anonymous sender must be refused");
    assert!(data["detail"].as_str().is_some_and(|d| d.contains("echo_gate")), "{data}");

    // signup on the leg: the account is prism's, and the answer is identity, not a verb.
    send_uplink(
        &mut ws,
        json!({"dest": "signup", "data": {"username": "ada", "password": "correct horse"}}),
    )
    .await;
    let (slot, name, data) = recv_speech(&mut ws).await;
    assert_eq!((slot.as_str(), name.as_str()), ("identity", "identity"));
    assert!(
        data["user"].as_str().is_some_and(|u| u.len() == 36),
        "identity carries the bound user as uuid text: {data}"
    );

    // Bound now: the same declared-auth event reaches the booth and its answer is DRAWN. The
    // gate is not merely a boolean on a log line — the frame proves the call ran.
    send_uplink(&mut ws, json!({"dest": "echo_gate", "data": op})).await;
    let ops = recv_ops(&mut ws).await;
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0]["slot"], "secret");

    // After logout the same event is refused again — the field, not the account row, is what
    // the gate reads, and it is cleared on the same socket.
    send_uplink(&mut ws, json!({"dest": "logout", "data": null})).await;
    let (slot, name, data) = recv_speech(&mut ws).await;
    assert_eq!((slot.as_str(), name.as_str()), ("identity", "identity"));
    assert!(data["user"].is_null(), "logout clears the user: {data}");
    send_uplink(&mut ws, json!({"dest": "echo_gate", "data": op})).await;
    let (slot, name, _) = recv_speech(&mut ws).await;
    assert_eq!((slot.as_str(), name.as_str()), ("notice", "notice"));
}

/// Revocation reaches BOTH legs: a revoked device's renderer socket is closed, not left
/// watching a page that still believes it is logged in.
#[tokio::test]
async fn revocation_kicks_the_renderer_leg_too() {
    let (addr, _gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let device = read_connected(&mut ws).await;

    // Bind an account so this connection carries an auth field worth clearing.
    send_uplink(&mut ws, json!({"dest": "signup", "data": {"username": "grace", "password": "p"}})).await;
    let (_, name, _) = recv_speech(&mut ws).await;
    assert_eq!(name, "identity");

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

/// DATA on the downlink: a `ui:<name>` whose payload is not an operation list goes into the
/// VALS plane under that name, exactly as if the page had emitted it — the operation list shape
/// (the test above) and the data shape are both legal, and which one applies is the emitter's
/// choice, not a second protocol.
#[tokio::test]
async fn a_view_event_carries_data_into_the_slot_named_by_it() {
    let (addr, gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let device = read_connected(&mut ws).await;
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "ui:identity",
        json!({"name": "ui:identity", "device": device, "data": {"device": device, "user": null}}),
    )
    .await
    .expect("emit ui:identity");

    let ops = recv_ops(&mut ws).await;
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0]["action"], "inject");
    // The marker is stripped: the page's slot is `identity`, and the name it sees is the same
    // name — nothing on the page knows what `ui:` means.
    assert_eq!(ops[0]["slot"], "identity");
    assert_eq!(ops[0]["name"], "identity");
    assert_eq!(ops[0]["data"]["device"].as_str(), Some(device.as_str()), "{ops:?}");
}

/// The sender does not hear its own emit: the fan-out is the channel minus whoever spoke.
#[tokio::test]
async fn a_view_event_does_not_echo_back_to_its_sender() {
    let (addr, gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let device = read_connected(&mut ws).await;
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "ui:chat",
        json!({"name": "ui:chat", "device": device, "sender": {"device": device}, "data": {"echo": true}}),
    )
    .await
    .expect("emit ui:chat");
    // Nothing comes back, so the next message on this socket is the answer to our own uplink.
    let op = json!({"action": "set", "slot": "graph", "data": {"type": "case", "attrs": {"class": ["mine"]}}});
    send_uplink(&mut ws, json!({"dest": "echo_frame", "data": op})).await;
    let ops = recv_ops(&mut ws).await;
    assert_eq!(ops[0]["data"]["attrs"]["class"][0], "mine", "{ops:?}");
}

/// A view event aimed at another device reaches nobody here: the fan-out is by device when the
/// payload names one.
#[tokio::test]
async fn a_view_event_for_another_device_is_not_drawn_here() {
    let (addr, gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    // The connection's own device is what the addressed-elsewhere event must NOT reach; only
    // the fact is needed here, and it is read to prove the leg is established.
    let _device = read_connected(&mut ws).await;
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "ui:graph",
        // Another device's address: any id that is not ours. Ids are opaque text, so "another
        // device" is simply another uuid — nothing about it is arithmetic.
        json!({"name": "ui:graph", "device": okm_core::Uuid7::now().to_string(), "data": {"mark": "theirs"}}),
    )
    .await
    .expect("emit view");
    // Ours, not theirs: the next message answers our own uplink, which proves nothing was
    // drawn from the other device's event.
    let op = json!({"action": "set", "slot": "graph", "data": {"type": "case", "attrs": {"class": ["mine"]}}});
    send_uplink(&mut ws, json!({"dest": "echo_frame", "data": op})).await;
    let ops = recv_ops(&mut ws).await;
    assert_eq!(ops[0]["data"]["attrs"]["class"][0], "mine", "{ops:?}");
}
