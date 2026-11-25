//! The renderer's leg, end to end: `/channel` speaks fluxen's wire — DOWNSTREAM AN ACTION
//! LIST (no envelope, fluxen 460d867), UPSTREAM `Outflow{event, id?, data}` — and everything
//! the VENUE has to say rides the same wire as an `inject` (fluxen ADR 0011), because a
//! renderer refuses a map outright: "frame is an envelope, not an action list".
//!
//! That refusal is why these tests never assert `frame["ev"]`: the leg has no verbs any more.
//! The venue's speech is `inject` into a slot named after the fact it states — `identity` (who am
//! I), `connect` (a connection opened, which the application answers with its layout) or `notice`
//! (what failed); everything else is the application's own operations.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

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
    boot_with_idle(None, deployment).await
}

/// The same venue with the idle window spelled out: 30 days is the deployment's answer, and a test
/// cannot wait for it.
async fn boot_with_idle(
    idle_ms: Option<u64>,
    deployment: Deployment,
) -> (std::net::SocketAddr, std::sync::Arc<Gateway>) {
    let dir = tempfile::tempdir().unwrap();
    let registry = match idle_ms {
        Some(ms) => Registry::open_with_idle(&dir.keep(), ms).unwrap(),
        None => Registry::open(&dir.keep()).unwrap(),
    };
    let engine = Engine::start(&Default::default())
        .await
        .expect("engine boot");
    // The login verbs ask the auth booth (a call); the gate test signs up, so it must be live.
    Gateway::with_auth(&engine, &registry).await.unwrap();
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::with_deployment(engine, registry, prism::nodes::Posture::Open, deployment);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = std::sync::Arc::clone(&gw);
    tokio::spawn(async move { served.serve(listener).await.unwrap() });
    (addr, gw)
}

/// The venue's two speeches for one landing are two emits, and the plane promises no order between
/// them (MODELING): a test that reads "the first speech" is reading its own luck. Collect both by
/// name and hand them back by name.
async fn recv_named(
    ws: &mut WsClient,
    wanted: [&str; 2],
) -> std::collections::BTreeMap<String, Value> {
    let mut got = std::collections::BTreeMap::new();
    while got.len() < wanted.len() {
        let (slot, name, data) = recv_speech(ws, &wanted).await;
        assert_eq!(slot, name, "the slot is named after the fact: {name}");
        assert!(wanted.contains(&name.as_str()), "unexpected fact `{name}`");
        got.insert(name, data);
    }
    got
}

/// The two facts the venue says to a connection that just opened: `token` (the credential the page
/// must keep — the one thing it cannot derive) and `connect` (the fact an application answers with
/// its layout). Both
/// are EVENTS on the plane like any other, so the page sees a slot named after the fact — there is
/// no per-verb `connected`/`signup.result` name to learn. They are two emits, and the plane promises
/// no order between them: a page binds both, and neither assumes the other arrived first.
async fn read_connected(ws: &mut WsClient) -> String {
    let mut identity_device: Option<String> = None;
    let mut connect_device: Option<String> = None;
    for _ in 0..2 {
        let (slot, name, data) = recv_speech(ws, &["token", "connect"]).await;
        assert_eq!(slot, name, "the slot is named after the fact: {name}");
        match name.as_str() {
            // The device id is a uuid: TEXT on this wire (a 128-bit id is not a JSON number) —
            // and the credential rides beside it, because a page that loses the token has to log
            // in again, while a page that loses the device id has lost nothing.
            "token" => {
                assert!(
                    data["token"].as_str().is_some_and(|t| t.len() == 36),
                    "{data}"
                );
                identity_device = Some(
                    data["device"]
                        .as_str()
                        .expect("device id in the token fact")
                        .to_string(),
                );
            }
            "connect" => {
                connect_device = Some(
                    data["device"]
                        .as_str()
                        .expect("the fact names the device")
                        .to_string(),
                );
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

/// The venue's own speech: one injected event into the slot named after the fact. The leg ALSO
/// carries application frames — the shipped auth booth answers `ui:login` with a screen, exactly as
/// any booth may — so this reader SKIPS what is not the venue's speech instead of asserting on
/// arrival order. A test that wants the booth's frames reads them directly.
async fn recv_speech(ws: &mut WsClient, want: &[&str]) -> (String, String, Value) {
    loop {
        let ops = recv_ops(ws).await;
        let [op] = ops.as_slice() else { continue };
        // A FACT is a value in the slot named after it, and so is a booth's answer: both are `set`s
        // now (fluxen ADR 0038 §1 deleted `inject`, the kind that told them apart), so the reader is
        // TOLD which facts it is waiting for and skips everything else on the leg. The slot IS the
        // fact's name — there is no second name to read, which is why it is reported twice.
        match op["action"].as_str() {
            Some("set") => {
                if let Some(slot) = op["slot"].as_str() {
                    if want.contains(&slot) {
                        return (slot.to_string(), slot.to_string(), op["data"].clone());
                    }
                }
            }
            // A NOTICE IS A ROW (say()'s rule): it is APPENDED to the `notice` rack with an id of
            // its own, because a rack holds every notice the device has seen while a plain slot
            // would keep only the last. The rack's slot IS the fact's name — there is no second
            // name to read, which is why this reader reports the slot twice.
            Some("append") => {
                if let Some(slot) = op["slot"].as_str() {
                    return (slot.to_string(), slot.to_string(), op["data"].clone());
                }
            }
            _ => continue,
        }
    }
}

async fn send_uplink(ws: &mut WsClient, value: Value) {
    ws.send(Ws::Text(serde_json::to_string(&value).unwrap()))
        .await
        .unwrap();
}

/// The layout document an application ships: a list of Accrete frames.
#[tokio::test]
async fn assets_are_plain_downloads_on_the_shared_loop() {
    let bundle = tempfile::tempdir().unwrap();
    std::fs::write(
        bundle.path().join("index.html"),
        "<!doctype html><div id=main></div>",
    )
    .unwrap();
    std::fs::write(bundle.path().join("ui_leptos-abc123.js"), "console.log(1)").unwrap();
    let (addr, _gw) = boot(Deployment {
        assets: Some(AssetRoot::open(bundle.path()).unwrap()),
        shell: None,
    })
    .await;

    // Hashed asset: content-typed and immutable for a year.
    let resp = get(addr, "/ui_leptos-abc123.js").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(
        resp.contains("Content-Type: text/javascript; charset=utf-8"),
        "{resp}"
    );
    assert!(resp.contains("immutable"), "{resp}");
    assert!(resp.ends_with("console.log(1)"), "{resp}");

    // The mount root is the entry document, revalidated (not immutable).
    let resp = get(addr, "/").await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(
        resp.contains("Content-Type: text/html; charset=utf-8"),
        "{resp}"
    );
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
                auth: false,
            }],
            concurrency: aura_booth::Concurrency::Queued,
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
    assert_eq!(
        fact["device"].as_str(),
        Some(device.as_str()),
        "the fact names the device: {fact}"
    );

    // The application's answer: frames the venue forwards AS the message (one action list).
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "ui:layout",
        json!({"name": "ui:layout", "device": device, "data": [
            {"action": "create", "slot": "", "data": {"type": "frame", "attrs": {"class": ["from-event"]}}},
            {"action": "set", "slot": "graph", "data": {"type": "frame"}},
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

/// The window, on the wire. A connection that nobody has used for longer than the window stops being
/// authenticated — the SOCKET stays (a reconnect is not the answer), the page is told why, and a
/// login on that same connection is what starts the uplinks again. The device keeps its id across
/// the timeout, which is the whole reason a page does not have to be renamed every time.
#[tokio::test]
async fn a_silent_connection_crosses_the_window_and_a_login_reopens_it() {
    let (addr, _gw) = boot_with_idle(Some(150), Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let device = read_connected(&mut ws).await;

    // Bind the device so the connection carries a user worth losing.
    send_uplink(
        &mut ws,
        json!({"dest": "signup", "data": {"login_name": "ada", "password": "correct horse"}}),
    )
    .await;
    let landed = recv_named(&mut ws, ["token", "login"]).await;
    assert_eq!(landed["login"]["device"].as_str(), Some(device.as_str()));

    // Silence past the window, then one uplink: it is refused with a reason.
    tokio::time::sleep(std::time::Duration::from_millis(320)).await;
    send_uplink(
        &mut ws,
        json!({"dest": "echo_steel", "data": {"text": "late"}}),
    )
    .await;
    let (slot, name, data) = recv_speech(&mut ws, &["notice"]).await;
    assert_eq!((slot.as_str(), name.as_str()), ("notice", "notice"));
    assert_eq!(
        data["reason"].as_str(),
        Some("idle"),
        "the page must know WHY: {data}"
    );

    // The auth verb is not gated: a login on the same connection names the SAME device.
    send_uplink(
        &mut ws,
        json!({"dest": "login", "data": {"login_name": "ada", "password": "correct horse"}}),
    )
    .await;
    let again = recv_named(&mut ws, ["token", "login"]).await;
    assert_eq!(
        again["login"]["device"].as_str(),
        Some(device.as_str()),
        "the device survives the timeout — that is what keeps its name"
    );
}

/// The other end of a connection is a fact too. `ui:disconnect {device}` is said BEFORE the table
/// forgets the device, so an application holding a conversation for it hears that it is over — and
/// the page's own socket is gone by then, which is why this is a fact for applications, not a
/// notice the venue puts on the wire for a browser.
#[tokio::test]
async fn a_disconnect_is_a_fact_for_the_application() {
    let (addr, gw) = boot(Deployment::default()).await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let collector = std::sync::Arc::clone(&seen);
    gw.engine
        .register(aura_booth::BoothType {
            name: "gone_probe".into(),
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
                event: "ui:disconnect".into(),
                entry: None,
                resolution: aura_booth::RouteResolution::Singleton,
                wildcard: false,
                auth: false,
            }],
            concurrency: aura_booth::Concurrency::Queued,
        })
        .await
        .expect("register the disconnect probe");

    let mut ws = open_ui(addr).await;
    let device = read_connected(&mut ws).await;
    // An explicit Close, not a drop: the server's read half learns of a drop only when the OS gets
    // around to the FIN, which is not something a test should wait on.
    ws.close(None).await.expect("close the renderer socket");

    let mut fact = None;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if let Some(first) = seen.lock().unwrap().first() {
            fact = Some(first.clone());
            break;
        }
    }
    let fact = fact.expect("ui:disconnect reached the plane");
    assert_eq!(
        fact["device"].as_str(),
        Some(device.as_str()),
        "the fact names the device that went away: {fact}"
    );
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
    assert!(
        frame.get("args").is_some(),
        "peer leg answers with args: {frame}"
    );
    assert!(
        frame.get("content").is_none(),
        "peer leg carries no Accrete: {frame}"
    );
}

/// Application uplinks reach the booth plane: the booth NAMED BY THE EVENT is called, and it is
/// called with the venue's envelope — `{sender: {device, user}, args: <uplink data>}`. Identity
/// rides the payload (ADR-0017 §9), never the instance id, so this is the only place a booth can
/// learn who asked; `echo_steel` hands that envelope straight back, which makes it readable here.
#[tokio::test]
async fn an_uplink_dispatches_to_the_booth_named_by_the_event() {
    let (addr, _gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    read_connected(&mut ws).await;
    // The booth's answer IS the operation the caller sent: an answer that is not an operation
    // is data, and data has no path down this leg — a booth that wants the page to see
    // something emits `ui:<name>`. So the drawn frame here proves the dispatch happened.
    let op = json!({"action": "set", "slot": "graph", "data": {"type": "frame", "attrs": {"class": ["echoed"]}}});
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
    // TWO facts on a refusal: the platform's (`auth.required`, which is what opens the surface — no
    // application learns of a dropped event) and the person's (`notice`). Both, in either order.
    let said = recv_named(&mut ws, ["auth.required", "notice"]).await;
    // The notice is a ROW (say()'s rule), so its words live where a text node's do:
    // `bind.value.default`. Reading `detail` here is what made this suite fail on the shape it
    // thought the wire had rather than the one it has.
    assert!(
        said["notice"]["bind"]["value"]["default"]
            .as_str()
            .is_some_and(|d| d.contains("echo_gate")),
        "the person is told: {said:?}"
    );
    assert_eq!(
        said["auth.required"]["event"], "echo_gate",
        "the refusal names what it dropped"
    );

    // signup on the leg: the account is prism's, and the answer is a fact, not a verb.
    send_uplink(
        &mut ws,
        json!({"dest": "signup", "data": {"login_name": "ada", "password": "correct horse"}}),
    )
    .await;
    let landed = recv_named(&mut ws, ["token", "login"]).await;
    let token = &landed["token"];
    assert!(
        token["token"].as_str().is_some_and(|t| t.len() == 36),
        "the rotated credential is what only this fact can carry: {token}"
    );
    assert!(
        token.get("user").is_none(),
        "the user is the venue's answer, not the page's: {token}"
    );
    // AND THE LIFECYCLE SAYS IT TOO (MODELING: facts are named as they happened) — `login` is the
    // fact an application binds, and it carries who bound.
    let login = &landed["login"];
    assert!(
        login["user"].as_str().is_some_and(|u| u.len() == 36),
        "{login}"
    );
    assert!(login["device"].as_str().is_some(), "{login}");

    // Bound now: the same declared-auth event reaches the booth and its answer is DRAWN. The
    // gate is not merely a boolean on a log line — the frame proves the call ran.
    send_uplink(&mut ws, json!({"dest": "echo_gate", "data": op})).await;
    // The shipped auth booth answers the login with its own screen (any booth subscribed to `ui:login`
    // may), so the drawing this test wants is the one whose slot is the gate's — skip the rest rather
    // than reading whatever arrived first.
    let ops = loop {
        let ops = recv_ops(&mut ws).await;
        if ops.iter().any(|o| o["slot"] == "secret") {
            break ops;
        }
    };
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0]["slot"], "secret");

    // After logout the same event is refused again — the field, not the account row, is what
    // the gate reads, and it is cleared on the same socket.
    send_uplink(&mut ws, json!({"dest": "logout", "data": null})).await;
    let gone = recv_named(&mut ws, ["token", "logout"]).await;
    assert!(
        gone["logout"]["device"].as_str().is_some(),
        "{:?}",
        gone["logout"]
    );
    send_uplink(&mut ws, json!({"dest": "echo_gate", "data": op})).await;
    let again = recv_named(&mut ws, ["auth.required", "notice"]).await;
    assert!(
        again["notice"]["bind"]["value"]["default"]
            .as_str()
            .is_some(),
        "refused again after the logout: {again:?}"
    );
}

/// Revocation reaches BOTH legs: a revoked device's renderer socket is closed, not left
/// watching a page that still believes it is logged in.
#[tokio::test]
async fn revocation_kicks_the_renderer_leg_too() {
    let (addr, _gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let device = read_connected(&mut ws).await;

    // Bind an account so this connection carries an auth field worth clearing.
    send_uplink(
        &mut ws,
        json!({"dest": "signup", "data": {"login_name": "grace", "password": "p"}}),
    )
    .await;
    recv_named(&mut ws, ["token", "login"]).await;

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
        "ui:token",
        json!({"name": "ui:token", "device": device, "data": {"device": device, "token": "t"}}),
    )
    .await
    .expect("emit ui:token");

    let ops = recv_ops(&mut ws).await;
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(ops[0]["action"], "set");
    // The marker is stripped: the page's slot is `token`, and that is the whole of what it is
    // told — nothing on the page knows what `ui:` means, and there is no second name on the wire.
    assert_eq!(ops[0]["slot"], "token");
    assert!(ops[0].get("name").is_none(), "{ops:?}");
    assert_eq!(
        ops[0]["data"]["device"].as_str(),
        Some(device.as_str()),
        "{ops:?}"
    );
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
    let op = json!({"action": "set", "slot": "graph", "data": {"type": "frame", "attrs": {"class": ["mine"]}}});
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
    let op = json!({"action": "set", "slot": "graph", "data": {"type": "frame", "attrs": {"class": ["mine"]}}});
    send_uplink(&mut ws, json!({"dest": "echo_frame", "data": op})).await;
    let ops = recv_ops(&mut ws).await;
    assert_eq!(ops[0]["data"]["attrs"]["class"][0], "mine", "{ops:?}");
}

/// A USER IS AN ADDRESS TOO, and only the venue can deliver one (ADR-0018 §3): a booth cannot
/// enumerate devices, so "reach that person" is a fan-out here. Two live devices of one account
/// both receive it, and a third connection that never logged in receives nothing — an account's
/// reach stops where the binding does.
#[tokio::test]
async fn a_user_address_reaches_every_live_device_of_that_account() {
    let (addr, gw) = boot(Deployment::default()).await;
    let mut one = open_ui(addr).await;
    let _one = read_connected(&mut one).await;
    send_uplink(
        &mut one,
        json!({"dest": "signup", "data": {"login_name": "ada", "password": "correct horse"}}),
    )
    .await;
    let landed = recv_named(&mut one, ["token", "login"]).await;
    let user = landed["login"]["user"]
        .as_str()
        .expect("the account the login bound")
        .to_string();

    // The same account on a second socket — a phone beside the laptop.
    let mut two = open_ui(addr).await;
    let _two = read_connected(&mut two).await;
    send_uplink(
        &mut two,
        json!({"dest": "login", "data": {"login_name": "ada", "password": "correct horse"}}),
    )
    .await;
    let second = recv_named(&mut two, ["token", "login"]).await;
    assert_eq!(
        second["login"]["user"].as_str(),
        Some(user.as_str()),
        "one account, two devices: {second:?}"
    );

    // A socket that never logs in: the account's fan-out must not reach it.
    let mut three = open_ui(addr).await;
    let _three = read_connected(&mut three).await;

    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "ui:sync",
        json!({"name": "ui:sync", "user": user, "data": {"mark": "for-ada"}}),
    )
    .await
    .expect("emit ui:sync");

    for (who, ws) in [("first", &mut one), ("second", &mut two)] {
        let given = tokio::time::timeout(std::time::Duration::from_secs(5), recv_speech(ws, &["sync"])).await;
        let (slot, name, data) =
            given.unwrap_or_else(|_| panic!("{who}: the fan-out reaches this device"));
        assert_eq!(
            (slot.as_str(), name.as_str()),
            ("sync", "sync"),
            "{who}: the slot and the fact are the event's own name"
        );
        assert_eq!(data["mark"], "for-ada", "{who}: {data}");
    }

    // The anonymous one: its next message answers its OWN uplink, which is what "nothing was
    // delivered here" looks like on a leg that has no silence.
    let op = json!({"action": "set", "slot": "graph", "data": {"type": "frame", "attrs": {"class": ["mine"]}}});
    send_uplink(&mut three, json!({"dest": "echo_frame", "data": op})).await;
    let ops = recv_ops(&mut three).await;
    assert_eq!(ops[0]["data"]["attrs"]["class"][0], "mine", "{ops:?}");
}

/// An address that cannot be read is NOT an absent one. `device: 12` used to fall through
/// `.and_then(as_str)` into the broadcast arm — a typo that reaches every screen in the venue is
/// the one failure this rule exists for.
#[tokio::test]
async fn a_malformed_address_reaches_nobody_rather_than_everybody() {
    let (addr, gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let _device = read_connected(&mut ws).await;
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "ui:typo",
        json!({"name": "ui:typo", "device": 12, "data": {"mark": "typo"}}),
    )
    .await
    .expect("emit ui:typo");
    let op = json!({"action": "set", "slot": "graph", "data": {"type": "frame", "attrs": {"class": ["mine"]}}});
    send_uplink(&mut ws, json!({"dest": "echo_frame", "data": op})).await;
    let ops = recv_ops(&mut ws).await;
    assert_eq!(
        ops[0]["data"]["attrs"]["class"][0], "mine",
        "a typo is not a broadcast: {ops:?}"
    );
}

/// The device list on the renderer's leg: the page asks, the venue answers with the FACT `devices`
/// (into the slot named after it), and a name rides back the same way. Anonymous is told through a
/// notice — the same refusal a program reads as an error value.
#[tokio::test]
async fn a_page_can_ask_for_its_accounts_devices() {
    let (addr, _gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    read_connected(&mut ws).await;

    send_uplink(&mut ws, json!({"dest": "devices", "data": {}})).await;
    let (slot, name, data) = recv_speech(&mut ws, &["notice"]).await;
    assert_eq!(
        (slot.as_str(), name.as_str()),
        ("notice", "notice"),
        "{data}"
    );

    send_uplink(
        &mut ws,
        json!({"dest": "signup", "data": {"login_name": "ada", "password": "correct horse"}}),
    )
    .await;
    recv_named(&mut ws, ["token", "login"]).await;

    send_uplink(&mut ws, json!({"dest": "devices", "data": {}})).await;
    let (slot, name, data) = recv_speech(&mut ws, &["devices"]).await;
    assert_eq!(
        (slot.as_str(), name.as_str()),
        ("devices", "devices"),
        "{data}"
    );
    let rows = data["devices"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "one device so far: {data}");
    assert_eq!(rows[0]["current"], true, "{data}");
    assert_eq!(
        rows[0]["alias"], "",
        "unnamed until the person names it: {data}"
    );

    send_uplink(
        &mut ws,
        json!({"dest": "alias", "data": {"alias": "laptop"}}),
    )
    .await;
    let (slot, name, data) = recv_speech(&mut ws, &["alias"]).await;
    assert_eq!((slot.as_str(), name.as_str()), ("alias", "alias"), "{data}");
    assert_eq!(data["alias"], "laptop", "{data}");

    // The list reads the name back — the rename was storage, not a receipt.
    send_uplink(&mut ws, json!({"dest": "devices", "data": {}})).await;
    let (_, _, data) = recv_speech(&mut ws, &["devices"]).await;
    assert_eq!(data["devices"][0]["alias"], "laptop", "{data}");
}

/// A STOP IS NOT AN EVENT (ADR-0008 as revised): a page's cancel is served by the VENUE,
/// which holds authority over the connection, by setting the realm's stop bit on the
/// sender's own instance. Nothing goes on the plane, so nothing can queue behind the turn
/// it means to stop — which is the failure the event shape had.
#[tokio::test]
async fn a_cancel_sets_the_stop_bit_on_the_senders_own_instance() {
    let (addr, gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let device = read_connected(&mut ws).await;

    let target = gravity_booth::turn_instance(&device);
    assert!(
        !gw.engine.realm.lock().await.stops.is_set(&target),
        "nothing asked yet"
    );

    send_uplink(&mut ws, json!({ "dest": "turn.cancel", "data": {} })).await;

    for _ in 0..200 {
        if gw.engine.realm.lock().await.stops.is_set(&target) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the venue never set the stop bit for {device}");
}

/// A frame for an account with no screen awake is HELD, not dropped: the venue is the only layer
/// that knows both the address and who is connected, so it holds the tail and hands it over the
/// moment a device of that account proves itself again. Nobody else ever sees it.
#[tokio::test]
async fn a_frame_for_an_asleep_account_waits_for_its_next_device() {
    let (addr, gw) = boot(Deployment::default()).await;
    let mut one = open_ui(addr).await;
    let _connected = read_connected(&mut one).await;
    send_uplink(
        &mut one,
        json!({"dest": "signup", "data": {"login_name": "ada", "password": "correct horse"}}),
    )
    .await;
    let landed = recv_named(&mut one, ["token", "login"]).await;
    let user = landed["login"]["user"]
        .as_str()
        .expect("the account")
        .to_string();

    // The account signs out: it exists, and no screen of it is awake.
    send_uplink(&mut one, json!({"dest": "logout", "data": {}})).await;
    for _ in 0..2 {
        // `token` and `logout` — drain them so the next reads start clean.
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(5), recv_speech(&mut one, &["token", "logout"])).await;
    }

    // Addressed to the account while nothing of theirs is awake.
    aura_realm::Realm::emit(
        &gw.engine.realm,
        None,
        "ui:sync",
        json!({"name": "ui:sync", "user": user, "data": {"mark": "while-asleep"}}),
    )
    .await
    .expect("emit ui:sync");

    // A neighbour that never signs in must not be handed the account's frames.
    let mut stranger = open_ui(addr).await;
    let _connected = read_connected(&mut stranger).await;

    // Signing in again settles the debt on that screen.
    send_uplink(
        &mut one,
        json!({"dest": "login", "data": {"login_name": "ada", "password": "correct horse"}}),
    )
    .await;

    let mut seen = None;
    for _ in 0..6 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), recv_speech(&mut one, &["sync"]))
            .await
            .expect("the held frame reaches the account's device");
        if frame.0 == "sync" {
            seen = Some(frame.2);
            break;
        }
    }
    assert_eq!(
        seen.as_ref().and_then(|d| d["mark"].as_str()),
        Some("while-asleep"),
        "the frame held for the account arrived: {seen:?}"
    );

    // The anonymous socket beside it heard nothing: a held frame is the account's, not the venue's.
    let op = json!({"action": "set", "slot": "graph", "data": {"type": "frame", "attrs": {"class": ["mine"]}}});
    send_uplink(&mut stranger, json!({"dest": "echo_frame", "data": op})).await;
    let ops = recv_ops(&mut stranger).await;
    assert_eq!(ops[0]["data"]["attrs"]["class"][0], "mine", "{ops:?}");
}

/// A replay starts at the last WHOLE screen, not at the oldest frame held: a screen is a value
/// that already contains everything before it, so the increments that led to it are not replayed.
#[tokio::test]
async fn a_replay_starts_at_the_last_whole_screen() {
    let (addr, gw) = boot(Deployment::default()).await;
    let mut one = open_ui(addr).await;
    let _connected = read_connected(&mut one).await;
    send_uplink(
        &mut one,
        json!({"dest": "signup", "data": {"login_name": "ada", "password": "correct horse"}}),
    )
    .await;
    let landed = recv_named(&mut one, ["token", "login"]).await;
    let user = landed["login"]["user"]
        .as_str()
        .expect("the account")
        .to_string();
    send_uplink(&mut one, json!({"dest": "logout", "data": {}})).await;
    for _ in 0..2 {
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(5), recv_speech(&mut one, &["token", "logout"])).await;
    }

    let emit = |name: &str, data: Value| {
        let name = name.to_string();
        let who = user.clone();
        let realm = &gw.engine.realm;
        async move {
            let event = format!("ui:{name}");
            aura_realm::Realm::emit(
                realm,
                None,
                &event,
                json!({"name": format!("ui:{name}"), "user": who, "data": data}),
            )
            .await
            .expect("emit a held frame");
        }
    };
    // An increment, then a whole screen (which contains it), then one more increment.
    emit(
        "inc_before",
        json!([{"action": "append", "slot": "rows", "data": {"id": "before-screen", "type": "frame"}}]),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    emit(
        "redraw",
        json!([
            {"action": "create", "slot": "", "data": {"type": "frame", "attrs": {"class": ["maximize"]}}},
            {"action": "append", "slot": "rows", "data": {"id": "inside-screen", "type": "frame"}},
        ]),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    emit(
        "inc_after",
        json!([{"action": "append", "slot": "rows", "data": {"id": "after-screen", "type": "frame"}}]),
    )
    .await;

    send_uplink(
        &mut one,
        json!({"dest": "login", "data": {"login_name": "ada", "password": "correct horse"}}),
    )
    .await;

    // The venue's own speech (the rotated token, the login fact) arrives on the same leg; the
    // replay is the operation lists, read as they come.
    let mut replayed: Vec<Value> = Vec::new();
    for _ in 0..8 {
        let Ok(ops) =
            tokio::time::timeout(std::time::Duration::from_secs(5), recv_ops(&mut one)).await
        else {
            break;
        };
        // The venue's own speech is a value in a slot that belongs to no island; the replay this
        // test reads is the operation lists that draw.
        if ops.iter().any(|op| {
            op["action"] == "set"
                && op["slot"]
                    .as_str()
                    .is_some_and(|s| !s.starts_with("island:"))
        }) {
            continue;
        }
        let text = serde_json::to_string(&ops).unwrap_or_default();
        replayed.push(Value::Array(ops));
        if text.contains("after-screen") {
            break;
        }
    }
    let text = serde_json::to_string(&replayed).unwrap();
    assert!(
        !text.contains("before-screen"),
        "an increment the screen already contains is not replayed: {text}"
    );
    assert!(
        text.contains("inside-screen"),
        "the screen is replayed: {text}"
    );
    assert!(
        text.contains("after-screen"),
        "what moved since is replayed: {text}"
    );
    let screen = replayed
        .iter()
        .find(|f| {
            serde_json::to_string(f)
                .unwrap_or_default()
                .contains("inside-screen")
        })
        .expect("the whole screen came back");
    assert_eq!(
        screen[0]["action"].as_str(),
        Some("create"),
        "the replay starts at the screen: {text}"
    );
}

/// A FRAME IS DRAWN ONCE PER EMIT (gravity's open item: "a duplicated patch frame, multiplicity
/// growing per turn"). The suspicion there is this fan-out, so measure the mechanism with one page
/// and nothing else in the way: three emits, three drawings, and the earlier ones do not come back
/// with the later ones. A vertex that accumulates destinations — one per turn, per subscription or
/// per connection — shows up here as 1, then 2, then 3 copies, and nowhere else in prism can.
#[tokio::test]
async fn one_emit_draws_exactly_one_frame_however_much_was_said_before() {
    let (addr, gw) = boot(Deployment::default()).await;
    let mut ws = open_ui(addr).await;
    let device = read_connected(&mut ws).await;

    for round in 0..3u64 {
        aura_realm::Realm::emit(
            &gw.engine.realm,
            None,
            "ui:graph",
            json!({
                "name": "ui:graph",
                "device": device,
                "data": { "action": "set", "slot": "graph", "data": { "mark": round } },
            }),
        )
        .await
        .expect("emit view");
        let ops = recv_ops(&mut ws).await;
        assert_eq!(
            ops.len(),
            1,
            "emit {} drew {} actions, not one: {ops:?}",
            round + 1,
            ops.len()
        );
        assert_eq!(ops[0]["slot"], "graph", "{ops:?}");
        // The fact is INJECTED into the slot: an object body becomes one action under `data`.
        assert_eq!(
            ops[0]["data"]["data"]["mark"], round,
            "in order, once each: {ops:?}"
        );
    }

    let extra =
        tokio::time::timeout(std::time::Duration::from_millis(300), recv_ops(&mut ws)).await;
    assert!(
        extra.is_err(),
        "a fourth frame arrived after three emits: {:?}",
        extra.ok()
    );
}

/// The transport's probe, on the wire. A renderer that has heard nothing asks `__ping__` (fluxen ADR
/// 0044 §2), and the answer is a bare `__pong__` map — the one frame a renderer swallows before any
/// decoder ever sees it. Three things are asserted at once, because all three are the same decision:
/// the probe is ANSWERED (an unanswered one is a page that closes its own socket every dead window
/// and reconnects forever), it is NOT an event (a venue that dispatches it asks a booth named
/// `__ping__`, which no one meant to write), and it is NOT activity: a page probing every five
/// seconds is not a person talking, so the idle window must run out anyway.
#[tokio::test]
async fn a_probe_is_answered_but_is_not_an_event_and_not_activity() {
    let (addr, gw) = boot_with_idle(Some(300), Deployment::default()).await;
    // A booth named exactly like the probe: if the probe were ever dispatched, this is what would
    // answer it, so the counter is how the test says it was not.
    let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&asked);
    gw.engine
        .register(aura_booth::BoothType {
            name: "__ping__".into(),
            body: aura_booth::Body::Rust(std::sync::Arc::new(move |_ctx, _args| {
                let counter = std::sync::Arc::clone(&counter);
                Box::pin(async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(Value::Null)
                })
            })),
            idle_ttl: None,
            max_exec: None,
            hooks: std::collections::BTreeMap::new(),
            receives: vec![aura_booth::ReceiveDecl {
                event: "ui:__ping__".into(),
                entry: None,
                resolution: aura_booth::RouteResolution::Singleton,
                wildcard: false,
                auth: false,
            }],
            concurrency: aura_booth::Concurrency::Queued,
        })
        .await
        .expect("register a booth named like the probe");

    let mut ws = open_ui(addr).await;
    read_connected(&mut ws).await;

    send_uplink(&mut ws, json!({ "dest": "__ping__", "data": null })).await;
    let pong = recv_json(&mut ws).await;
    assert_eq!(
        pong["dest"], "__pong__",
        "the probe is answered on the wire: {pong}"
    );
    assert!(
        pong.get("action").is_none() && pong.get("slot").is_none(),
        "the answer is a transport frame, not an action list: {pong}"
    );
    // Nothing follows it: no refusal, no notice, and no answer from the booth named like the probe.
    let quiet =
        tokio::time::timeout(std::time::Duration::from_millis(200), recv_json(&mut ws)).await;
    assert!(
        quiet.is_err(),
        "the probe produced nothing else: {:?}",
        quiet.ok()
    );
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no booth is asked about a probe"
    );

    // Past the window now: a real uplink is refused exactly as it would have been without the ping,
    // which is what proves the probe did not refresh the clock.
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    send_uplink(
        &mut ws,
        json!({ "dest": "echo_steel", "data": { "text": "late" } }),
    )
    .await;
    let (slot, name, data) = recv_speech(&mut ws, &["notice"]).await;
    assert_eq!((slot.as_str(), name.as_str()), ("notice", "notice"));
    assert_eq!(
        data["reason"].as_str(),
        Some("idle"),
        "the probe did not hold the window open: {data}"
    );
}
