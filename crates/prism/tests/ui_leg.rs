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

async fn boot(deployment: Deployment) -> (SocketAddr, std::sync::Arc<Gateway>) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.into_path()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::with_deployment(engine, registry, prism::nodes::Posture::Open, deployment);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = std::sync::Arc::clone(&gw);
    tokio::spawn(async move { served.serve(listener).await.unwrap() });
    (addr, gw)
}

/// A deployment with no layout source answers `init.error` on connect — the model has no
/// client-asked `init`, so the refusal must be pushed, not waited for.
async fn connect_and_expect_no_layout(ws: &mut WsClient) -> u64 {
    let hello = recv_json(ws).await;
    assert_eq!(hello["ev"], "connected", "{hello}");
    let refused = recv_json(ws).await;
    assert_eq!(refused["ev"], "init.error", "{refused}");
    hello["sender"].as_str().unwrap().strip_prefix("device:").unwrap().parse().unwrap()
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
    // The connect answer rides the renderer's own shape: `ev != "draw"`, so a renderer
    // ignores it and a client that cares can read the sender.
    let hello = recv_json(&mut ws).await;
    assert_eq!(hello["ev"], "connected");
    assert!(hello["sender"].as_str().unwrap().starts_with("device:"));
    let empty = &hello["content"];
    assert!(empty.is_null() || empty.as_array().is_some_and(|a| a.is_empty()), "{hello}");

    // The layout follows ON CONNECT: no `init` uplink exists, so nothing is sent here.
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
    let (addr, _gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    connect_and_expect_no_layout(&mut ws).await;
    // The socket stays up: the next uplink gets its own answer.
    send_uplink(&mut ws, json!({"event": "echo_steel", "data": {}})).await;
    assert_eq!(recv_json(&mut ws).await["ev"], "echo_steel.result");
}

/// Two legs, two shapes: `/ws` keeps `{ev, args}` while `/ui` speaks the renderer's wire.
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

/// Application uplinks reach the booth plane: the booth NAMED BY THE EVENT is called, and
/// its answer comes back as frames (a terminator at minimum). `echo_steel` answers with the
/// envelope, which is not an Accrete operation — the leg skips it, logs it, and still
/// terminates the answer. The point under test is the DISPATCH, not the echo's shape.
#[tokio::test]
async fn an_uplink_dispatches_to_the_booth_named_by_the_event() {
    let (addr, _gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    connect_and_expect_no_layout(&mut ws).await;
    send_uplink(&mut ws, json!({"event": "echo_steel", "data": {"msg": "hi"}})).await;
    let done = recv_json(&mut ws).await;
    assert_eq!(done["ev"], "echo_steel.result", "{done}");
}

/// The per-event auth declaration is enforced on THIS leg too, with the same surface: a
/// booth that declares `auth` refuses an anonymous renderer, and the socket stays up.
#[tokio::test]
async fn the_auth_declaration_gates_the_renderer_leg() {
    let (addr, _gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    connect_and_expect_no_layout(&mut ws).await;

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
    let (addr, _gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let device = connect_and_expect_no_layout(&mut ws).await;

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

/// The layout source that is an EVENT: connect emits it, the application decides what to do,
/// and whatever it emits as `view` reaches the renderer. The venue never waits for an answer
/// and never calls the application — it emits, then forwards.
#[tokio::test]
async fn an_event_source_emits_and_forwards_the_applications_view_frames() {
    let (addr, gw) = boot(Deployment {
        assets: None,
        init: Some(InitSource::parse("event:ui_init").expect("event source")),
    })
    .await;
    let mut ws = open_ui(addr).await;
    let hello = recv_json(&mut ws).await;
    assert_eq!(hello["ev"], "connected", "{hello}");
    let device: u64 = hello["sender"].as_str().unwrap().strip_prefix("device:").unwrap().parse().unwrap();

    // Nothing follows on its own: the venue emitted `ui_init` and is waiting on the
    // application, which in this test is the test.
    let emitted = json!({"device": device});
    aura_realm::Realm::emit(&gw.engine.realm, None, "ui_init", emitted).await.expect("emit");

    // The application's answer, as the venue sees it: one `view` event carrying frames.
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "view",
        json!({"device": device, "frames": [
            {"action": "create", "event": "", "data": {"type": "case", "attrs": {"class": ["from-event"]}}},
            {"action": "set", "event": "graph", "data": {"type": "case"}},
        ]}),
    )
    .await
    .expect("emit view");

    let first = recv_json(&mut ws).await;
    assert_eq!(first["ev"], "draw", "{first}");
    assert_eq!(first["content"]["action"], "create");
    assert_eq!(first["content"]["data"]["attrs"]["class"][0], "from-event");
    let second = recv_json(&mut ws).await;
    assert_eq!(second["content"]["action"], "set");
    assert_eq!(second["content"]["event"], "graph");
}

/// A view event aimed at another device reaches nobody here: the fan-out is by device when
/// the payload names one.
#[tokio::test]
async fn a_view_event_for_another_device_is_not_drawn_here() {
    let (addr, gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let device = connect_and_expect_no_layout(&mut ws).await;
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "view",
        json!({"device": device + 999, "frames": [{"action": "set", "event": "graph", "data": {"type": "case"}}]}),
    )
    .await
    .expect("emit view");
    // Ours, not theirs: the next frame on this socket is an answer to our own uplink,
    // which proves nothing was drawn from the other device's event.
    send_uplink(&mut ws, json!({"event": "echo_steel", "data": {}})).await;
    assert_eq!(recv_json(&mut ws).await["ev"], "echo_steel.result");
}

// ---- the turn fixture -------------------------------------------------------
// The same stubs `turn_e2e` uses (an SSE provider and a mem-server), copied rather than
// shared: two test binaries cannot share a module without turning both into one, and the
// duplication is thirty lines of stub. `TurnConfig` rides the PROCESS environment, so one
// test per process may hold the serial guard for its whole body.

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

/// The turn on the RENDERER leg, end to end: the caller declares the slots, gravity builds
/// the frames, the venue forwards them, and nothing about a delta reaches the browser.
#[tokio::test]
async fn a_turn_on_the_renderer_leg_arrives_as_view_frames() {
    let _guard = serial().await;
    let (addr, _gw) = turn_boot(&["Hello", "world"]).await;
    let mut ws = open_ui(addr).await;
    connect_and_expect_no_layout(&mut ws).await;

    // The turn type declares `turn` user-gated: bind an account on this same socket.
    send_uplink(&mut ws, json!({"event": "signup", "data": {"username": "ada", "password": "p"}})).await;
    assert_eq!(recv_json(&mut ws).await["ev"], "signup.result");

    send_uplink(&mut ws, json!({"event": "turn.submit", "data": {"content": "hi"}})).await;

    let mut appends: Vec<String> = Vec::new();
    let mut sets: Vec<String> = Vec::new();
    let mut patched = String::new();
    let mut done = false;
    for _ in 0..60 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(20), recv_json(&mut ws))
            .await
            .expect("the turn must not stall");
        if frame["ev"] == "turn.done" {
            done = true;
            break;
        }
        assert_eq!(frame["ev"], "draw", "only frames belong on this leg: {frame}");
        match frame["content"]["action"].as_str().unwrap_or("") {
            "append" => appends.push(frame["content"]["event"].as_str().unwrap_or("").to_string()),
            "set" => sets.push(frame["content"]["event"].as_str().unwrap_or("").to_string()),
            "patch" => {
                patched.push_str(frame["content"]["value"].as_str().unwrap_or(""));
            }
            other => panic!("unexpected action `{other}`: {frame}"),
        }
    }
    assert!(done, "the turn never finished");
    // The question, then the answer bubble, both on the venue's declared rack.
    assert_eq!(appends, vec!["chat".to_string(), "chat".to_string()], "{appends:?}");
    // The streaming region is the venue's OTHER declared slot.
    assert_eq!(sets, vec!["say".to_string()], "{sets:?}");
    // Every provider token landed through a patch, in order.
    assert!(patched.contains("Hello") && patched.contains("world"), "{patched:?}");
}
