//! The renderer's leg, end to end: `/channel` speaks fluxen's wire — DOWNSTREAM AN ACTION
//! LIST (no envelope, fluxen 460d867), UPSTREAM `Outflow{event, id?, data}` — and everything
//! the VENUE has to say rides the same wire as an `inject` (fluxen ADR 0011), because a
//! renderer refuses a map outright: "frame is an envelope, not an action list".
//!
//! That refusal is why these tests never assert `frame["ev"]`: the leg has no verbs any more.
//! The venue's speech is `inject` into `identity` (who am I) or `notice` (what failed, a turn
//! ended); everything else is the application's own operations.
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

/// A deployment with no layout source answers `notice`/`init.error` on connect — the model has
/// no client-asked `init`, so the refusal is PUSHED, not waited for. Returns the device the
/// venue stated in `identity`.
async fn connect_and_expect_no_layout(ws: &mut WsClient) -> String {
    let device = read_connected(ws).await;
    let (slot, name, data) = recv_speech(ws).await;
    assert_eq!(slot, "notice", "a venue without a layout says so");
    assert_eq!(name, "notice", "{slot} {name}");
    assert!(data["detail"].as_str().is_some_and(|d| d.contains("layout")), "{data}");
    device
}

/// The connect answer: the `identity` fact, carrying the device the venue assigned. The fact is
/// said as an EVENT like any other (`view:identity`), so the page sees a slot named after it —
/// there is no per-verb `connected`/`signup.result` name to learn.
async fn read_connected(ws: &mut WsClient) -> String {
    let (slot, name, data) = recv_speech(ws).await;
    assert_eq!(slot, "identity", "connect answers identity");
    assert_eq!(name, "identity", "{name}");
    // The device id is a uuid: TEXT on this wire (a 128-bit id is not a JSON number), and the same
    // fact also carries the token the page must keep.
    assert!(data["token"].as_str().is_some_and(|t| t.len() == 36), "{data}");
    data["device"].as_str().expect("device id in identity").to_string()
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
fn layout_file() -> tempfile::NamedTempFile {
    let f = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        f.path(),
        "- action: create\n  slot: \"\"\n  data: {type: case, attrs: {}}\n\
         - action: set\n  slot: chat\n  data: {type: case}\n",
    )
    .unwrap();
    f
}

#[tokio::test]
async fn assets_are_plain_downloads_on_the_shared_loop() {
    let bundle = tempfile::tempdir().unwrap();
    std::fs::write(bundle.path().join("index.html"), "<!doctype html><div id=main></div>").unwrap();
    std::fs::write(bundle.path().join("ui_leptos-abc123.js"), "console.log(1)").unwrap();
    let (addr, _gw) = boot(Deployment {
        assets: Some(AssetRoot::open(bundle.path()).unwrap()),
        init: None,
    })
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

/// The layout arrives as ONE action list on connect: the whole document, in order, and no
/// terminator — there is no verb vocabulary left to terminate with.
#[tokio::test]
async fn init_delivers_the_layout_as_view_frames() {
    let layout = layout_file();
    let spec = format!("file:{}", layout.path().display());
    let (addr, _gw) = boot(Deployment {
        assets: None,
        init: Some(InitSource::parse(&spec).expect("file source")),
    })
    .await;

    let mut ws = open_ui(addr).await;
    read_connected(&mut ws).await;

    let ops = recv_ops(&mut ws).await;
    assert_eq!(ops.len(), 2, "the layout is one list: {ops:?}");
    assert_eq!(ops[0]["action"], "create");
    assert_eq!(ops[0]["data"]["type"], "case");
    assert_eq!(ops[1]["action"], "set");
    assert_eq!(ops[1]["slot"], "chat");
}

#[tokio::test]
async fn a_deployment_without_init_answers_instead_of_hanging() {
    let (addr, _gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    connect_and_expect_no_layout(&mut ws).await;
    // The socket stays up and still answers: the venue said it has no layout, and the next
    // uplink is dispatched normally.
    let op = json!({"action": "set", "slot": "graph", "data": {"type": "case"}});
    send_uplink(&mut ws, json!({"dest": "echo_frame", "data": op})).await;
    let ops = recv_ops(&mut ws).await;
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0]["slot"], "graph");
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
    connect_and_expect_no_layout(&mut ws).await;
    // The booth's answer IS the operation the caller sent: an answer that is not an operation
    // is data, and data has no path down this leg — a booth that wants the page to see
    // something emits `view:<name>`. So the drawn frame here proves the dispatch happened.
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
    connect_and_expect_no_layout(&mut ws).await;

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
    let device = connect_and_expect_no_layout(&mut ws).await;

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

/// The layout source that is an EVENT: connect emits it, the application decides what to do,
/// and whatever it emits as `view` reaches the renderer — as one action list, because that is
/// what a message is on this leg.
#[tokio::test]
async fn an_event_source_emits_and_forwards_the_applications_view_frames() {
    let (addr, gw) = boot(Deployment {
        assets: None,
        init: Some(InitSource::parse("event:ui_init").expect("event source")),
    })
    .await;
    let mut ws = open_ui(addr).await;
    let device = read_connected(&mut ws).await;

    // Nothing follows on its own: the venue emitted `ui_init` and is waiting on the
    // application, which in this test is the test.
    aura_realm::Realm::emit(&gw.engine.realm, None, "ui_init", json!({"device": device}))
        .await
        .expect("emit");

    // The application's answer, as the venue sees it: one `view` event carrying frames.
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "view:layout",
        json!({"name": "view:layout", "channel_id": device, "data": [
            {"action": "create", "slot": "", "data": {"type": "case", "attrs": {"class": ["from-event"]}}},
            {"action": "set", "slot": "graph", "data": {"type": "case"}},
        ]}),
    )
    .await
    .expect("emit view");

    let ops = recv_ops(&mut ws).await;
    assert_eq!(ops.len(), 2, "both frames in one list: {ops:?}");
    assert_eq!(ops[0]["action"], "create");
    assert_eq!(ops[0]["data"]["attrs"]["class"][0], "from-event");
    assert_eq!(ops[1]["action"], "set");
    assert_eq!(ops[1]["slot"], "graph");
}

/// DATA on the downlink: a `view:<name>` whose payload is not an operation list goes into the
/// VALS plane under that name, exactly as if the page had emitted it — the operation list shape
/// (the test above) and the data shape are both legal, and which one applies is the emitter's
/// choice, not a second protocol.
#[tokio::test]
async fn a_view_event_carries_data_into_the_slot_named_by_it() {
    let (addr, gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let device = connect_and_expect_no_layout(&mut ws).await;
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "view:identity",
        json!({"name": "view:identity", "channel_id": device, "data": {"device": device, "user": null}}),
    )
    .await
    .expect("emit view:identity");

    let ops = recv_ops(&mut ws).await;
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0]["action"], "inject");
    // The marker is stripped: the page's slot is `identity`, and the name it sees is the same
    // name — nothing on the page knows what `view:` means.
    assert_eq!(ops[0]["slot"], "identity");
    assert_eq!(ops[0]["name"], "identity");
    assert_eq!(ops[0]["data"]["device"].as_str(), Some(device.as_str()), "{ops:?}");
}

/// The sender does not hear its own emit: the fan-out is the channel minus whoever spoke.
#[tokio::test]
async fn a_view_event_does_not_echo_back_to_its_sender() {
    let (addr, gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let device = connect_and_expect_no_layout(&mut ws).await;
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "view:chat",
        json!({"name": "view:chat", "channel_id": device, "sender": {"device": device}, "data": {"echo": true}}),
    )
    .await
    .expect("emit view:chat");
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
    let device = connect_and_expect_no_layout(&mut ws).await;
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "view:graph",
        // Another device's address: any id that is not ours. Ids are opaque text, so "another
        // device" is simply another uuid — nothing about it is arithmetic.
        json!({"name": "view:graph", "channel_id": okm_core::Uuid7::now().to_string(), "data": {"mark": "theirs"}}),
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

// ---- the turn fixture -------------------------------------------------------
// The same stubs `turn_e2e` uses (an SSE provider and a mem-server), copied rather than
// shared: two test binaries cannot share a module without turning both into one, and the
// duplication is thirty lines of stub. `TurnConfig` rides the PROCESS environment, so one test
// per process holds the serial guard for its whole body.

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL.lock().await
}

async fn sse_server(words: &[&str]) -> String {
    let words: Vec<String> = words.iter().map(|w| w.to_string()).collect();
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(move || {
            let words = words.clone();
            async move {
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
            }
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

/// Boot a venue whose turn facts point at the local stubs (gravity's guest binaries are the
/// path-adjacent repo's debug build — build gravity before running this).
async fn turn_boot(words: &[&str]) -> (SocketAddr, std::sync::Arc<Gateway>) {
    let gravity_bins = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../gravity/target/debug");
    std::env::set_var("GRAVITY_PROVIDER_BIN", format!("{gravity_bins}/gravity-provider"));
    std::env::set_var("GRAVITY_TURN_BIN", format!("{gravity_bins}/gravity-turn"));
    let endpoint = sse_server(words).await;
    let store_url = mem_server().await;
    std::env::set_var("GRAVITY_STORE_URL", &store_url);
    std::env::set_var("GRAVITY_ENDPOINT", &endpoint);
    std::env::set_var("GRAVITY_MODEL", "test-model");
    std::env::set_var("GRAVITY_KEY_ENV", "GRAVITY_TEST_KEY");
    std::env::set_var("GRAVITY_TEST_KEY", "test-key");
    boot(Deployment::default()).await
}

/// The turn on the RENDERER leg, end to end: the caller declares the slots, gravity builds the
/// frames, the venue forwards them as action lists, and nothing about a delta reaches here.
#[tokio::test]
async fn a_turn_on_the_renderer_leg_arrives_as_view_frames() {
    let _guard = serial().await;
    let (addr, _gw) = turn_boot(&["Hello", "world"]).await;
    let mut ws = open_ui(addr).await;
    connect_and_expect_no_layout(&mut ws).await;

    // The turn type declares `turn` user-gated: bind an account on this same socket.
    send_uplink(&mut ws, json!({"dest": "signup", "data": {"username": "ada", "password": "p"}})).await;
    let (_, name, _) = recv_speech(&mut ws).await;
    assert_eq!(name, "identity");

    send_uplink(&mut ws, json!({"dest": "turn.submit", "data": {"content": "hi"}})).await;

    let mut appends: Vec<String> = Vec::new();
    let mut sets: Vec<String> = Vec::new();
    let mut patched = String::new();
    let mut done = false;
    for _ in 0..60 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(20), recv_json(&mut ws))
            .await
            .expect("the turn must not stall");
        let ops = frame
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("not an action list: {frame}"));
        for op in ops {
            if op["action"] == "inject" && op["slot"] == "turn.done" {
                done = true;
                continue;
            }
            match op["action"].as_str().unwrap_or("") {
                // fluxen's tree address is `slot` (460d867): an event is the uplink's name,
                // never an address inside the tree.
                "append" => appends.push(op["slot"].as_str().unwrap_or("").to_string()),
                "set" => sets.push(op["slot"].as_str().unwrap_or("").to_string()),
                "patch" => patched.push_str(op["value"].as_str().unwrap_or("")),
                other => panic!("unexpected action `{other}`: {op}"),
            }
        }
        if done {
            break;
        }
    }
    assert!(done, "the turn never finished");
    // The question, then the answer bubble, both on the venue's declared rack.
    assert_eq!(appends, vec!["chat".to_string(), "chat".to_string()], "{appends:?}");
    // The streaming region is the venue's OTHER declared slot, numbered by turn.
    assert_eq!(sets, vec!["say.1".to_string()], "{sets:?}");
    // Every provider token landed through a patch, in order.
    assert!(patched.contains("Hello") && patched.contains("world"), "{patched:?}");

    // ---- a SECOND turn on the same connection --------------------------------
    // The region must be a NEW name. Reusing one would make this turn's patches rewrite the
    // text inside the first turn's bubble: every frame correct, the conversation smeared.
    send_uplink(&mut ws, json!({"dest": "turn.submit", "data": {"content": "again"}})).await;
    let mut sets2: Vec<String> = Vec::new();
    let mut patched2 = String::new();
    let mut done2 = false;
    for _ in 0..60 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(20), recv_json(&mut ws))
            .await
            .expect("the second turn must not stall");
        let ops = frame
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("not an action list: {frame}"));
        for op in ops {
            if op["action"] == "inject" && op["slot"] == "turn.done" {
                done2 = true;
                continue;
            }
            match op["action"].as_str().unwrap_or("") {
                "set" => sets2.push(op["slot"].as_str().unwrap_or("").to_string()),
                "patch" => {
                    assert_eq!(op["slot"], "say.2", "turn 2 patches its OWN region: {op}");
                    patched2.push_str(op["value"].as_str().unwrap_or(""));
                }
                "append" => {}
                other => panic!("unexpected action `{other}`: {op}"),
            }
        }
        if done2 {
            break;
        }
    }
    assert!(done2, "the second turn never finished");
    assert_eq!(sets2, vec!["say.2".to_string()], "{sets2:?}");
    assert!(!patched2.is_empty(), "the second turn streamed nothing");
}
