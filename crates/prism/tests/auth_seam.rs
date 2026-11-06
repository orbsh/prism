//! The auth seam (ADR-0017 §2/§9): prism owns the verbs, the binding and the token; a BOOTH proves
//! the credential. Two facts are worth locking here, and neither is visible from the default boot:
//!
//! - a venue with NO auth booth is a legal anonymous plane — every login attempt answers the
//!   standard error naming the missing type, and the rest of the leg keeps working;
//! - the booth is replaceable: a different `auth` type answers the same call and prism binds the
//!   user id IT named, without a line of prism changing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use aura_engine::Engine;
use futures_util::{SinkExt, StreamExt};
use prism::{identity::Registry, Gateway};
use prism_protocol::{Codec, Frame};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Boot with the SHIPPED booth registered — the binary's boot path, which is what the surface test
/// needs (the store and the subscriptions both live there).
async fn boot_shipped() -> SocketAddr {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_auth(&engine, &registry).await.unwrap();
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, prism::nodes::Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });
    addr
}

/// The renderer's leg: upstream `Outflow`, downstream an array of operations.
async fn open_ui(addr: SocketAddr) -> WsClient {
    let url = format!("ws://{addr}/channel?codec=json");
    let (ws, _) = tokio_tungstenite::connect_async(url).await.expect("renderer handshake");
    ws
}

async fn send_uplink(ws: &mut WsClient, value: serde_json::Value) {
    ws.send(Message::Text(value.to_string())).await.unwrap();
}

/// One message = one action list. A map here is exactly what the real renderer refuses, so the helper
/// panics on one instead of quietly matching a shape only this test speaks.
async fn recv_ops(ws: &mut WsClient) -> Vec<serde_json::Value> {
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("the venue said nothing within 5s")
            .expect("stream ended")
            .expect("ws msg");
        let bytes = match msg {
            Message::Text(t) => t.into_bytes(),
            Message::Binary(b) => b,
            Message::Close(_) => panic!("server closed"),
            _ => continue,
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json frame");
        match value {
            serde_json::Value::Array(ops) => return ops,
            other => panic!("the renderer leg carries an action list, not {other}"),
        }
    }
}

/// Read until a `set` into the auth region whose form submits to `dest` arrives. Everything else on
/// the leg (the connect facts, the token, the login notice) is skipped by name, because the plane
/// promises no order between two emits.
async fn screen_into(ws: &mut WsClient, dest: &str) -> serde_json::Value {
    for _ in 0..12 {
        let ops = recv_ops(ws).await;
        for op in ops {
            if op["action"] != "set" || op["slot"] != prism::auth::AUTH_REGION {
                continue;
            }
            let node = &op["data"];
            if node["bind"]["value"]["dest"] == dest {
                return node.clone();
            }
        }
    }
    panic!("no screen submitting to `{dest}` arrived");
}

/// THE SURFACE THE BOOTH OWES (N2): a request draws the form into the region, and the venue's own
/// `ui:login` fact turns it into the signed-in line — the whole reason the proof and the login can
/// live in different places.
#[tokio::test]
async fn the_booth_draws_the_login_screen_and_the_signed_in_line() {
    let addr = boot_shipped().await;
    let mut ws = open_ui(addr).await;

    // The seeker is an ordinary uplink with the BARE name; the venue adds the marker.
    send_uplink(&mut ws, serde_json::json!({"dest": "auth.request", "data": {"mode": "login"}})).await;
    let login = screen_into(&mut ws, "login").await;
    let kinds: Vec<&str> = login["children"]
        .as_array()
        .expect("a form has children")
        .iter()
        .filter_map(|c| c["type"].as_str())
        .collect();
    assert_eq!(kinds, vec!["text", "input", "input", "button"], "a heading, two fields, a submit");
    assert_eq!(login["children"][2]["bind"]["value"]["type"], "password", "the secret is a password field");

    // Signing up is the same screen with one more field, and the venue's answer lands as the
    // signed-in line — the booth reads its own account row for the name. The payload is written the
    // way a PAGE writes it: every field is `{data, payload}` (fluxen's form shape), not a bare string.
    // A venue that read only the flat shape refused real pages with "needs `username`".
    send_uplink(
        &mut ws,
        serde_json::json!({"dest": "signup", "data": {
            "username": {"data": "ada", "payload": null},
            "password": {"data": "pw-ada", "payload": null},
            "nickname": {"data": "Ada", "payload": null},
        }}),
    )
    .await;
    let signed = screen_into(&mut ws, "logout").await;
    assert_eq!(signed["children"][0]["bind"]["value"]["default"], "signed in as Ada");
    assert_eq!(signed["children"][1]["type"], "button", "one control ends the binding");
}

/// Boot a venue that registers the echoes and NOTHING ELSE — no auth booth. Returns the address plus
/// the engine, so a test can install a booth of its own afterwards (registration is before serve).
async fn boot() -> (SocketAddr, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine.clone(), registry, prism::nodes::Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });
    (addr, engine)
}

/// A stand-in for a swapped implementation: it answers the same two operations from memory and mints
/// its own user id. Nothing here knows prism's storage — that is the point of the seam.
async fn install_sentinel(engine: &Engine, password: &str) -> prism::identity::UserId {
    let user = prism::identity::UserId::now();
    let password = password.to_string();
    let booth = aura_booth::BoothType {
        name: prism::auth::AUTH_BOOTH.to_string(),
        body: aura_booth::Body::Rust(Arc::new(move |_ctx, args| {
            let password = password.clone();
            Box::pin(async move {
                let op = args.get("op").and_then(serde_json::Value::as_str).unwrap_or("");
                let asked = args.get("password").and_then(serde_json::Value::as_str).unwrap_or("");
                match op {
                    "verify" | "register" if asked == password => {
                        Ok(serde_json::json!({ "user": user.to_string() }))
                    }
                    "verify" | "register" => Err(anyhow::anyhow!("invalid credentials")),
                    other => Err(anyhow::anyhow!("unknown auth operation `{other}`")),
                }
            })
        })),
        idle_ttl: None,
        max_exec: None,
        hooks: std::collections::BTreeMap::new(),
        receives: Vec::new(),
        concurrency: aura_booth::Concurrency::Queued,
    };
    engine.register(booth).await.unwrap();
    user
}

async fn connect(addr: SocketAddr, query: &str) -> (WsClient, serde_json::Value) {
    let url = format!("ws://{addr}/ws?protocol=json{query}");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("ws handshake");
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "connected");
    (ws, f.args)
}

async fn send(ws: &mut WsClient, frame: &Frame) {
    ws.send(Message::Text(serde_json::to_string(frame).unwrap())).await.unwrap();
}

async fn recv(ws: &mut WsClient) -> Frame {
    loop {
        match ws.next().await.expect("stream ended").expect("ws msg") {
            Message::Binary(b) => return Codec::Json.decode(&b).unwrap(),
            Message::Text(t) => return Codec::Json.decode(t.as_bytes()).unwrap(),
            Message::Close(_) => panic!("server closed"),
            _ => continue,
        }
    }
}

/// NO BOOTH, NO LOGIN — AND STILL A PLANE. A deployment that never registered an `auth` type serves
/// anonymous traffic perfectly; the two verbs that need a proof say so instead of hanging.
#[tokio::test]
async fn a_venue_without_an_auth_booth_refuses_login_and_still_serves() {
    let (addr, _engine) = boot().await;
    let (mut ws, _) = connect(addr, "").await;

    for ev in ["login", "signup"] {
        send(&mut ws, &Frame::new(ev, serde_json::json!({"username": "a", "password": "b"}))).await;
        let f = recv(&mut ws).await;
        assert_eq!(f.ev, "error", "a login with no booth is an error VALUE, not a close");
        let message = f.args["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("auth booth"),
            "the refusal names what is missing: {message}"
        );
    }

    // The rest of the leg never needed a proof: the socket is up and the venue answers.
    send(&mut ws, &Frame::new("ping", serde_json::json!({}))).await;
    assert_eq!(recv(&mut ws).await.ev, "ping.result");
}

/// THE BOOTH IS REPLACEABLE: a different `auth` type answers the call, prism binds the user id IT
/// named, and the credential survives a reconnect through the rotated token.
#[tokio::test]
async fn a_swapped_auth_booth_names_the_user_and_prism_binds_it() {
    let (addr, engine) = boot().await;
    let user = install_sentinel(&engine, "open sesame").await;
    let (mut ws, ident) = connect(addr, "").await;
    let device = ident["device_id"].as_str().expect("a device id is issued").to_string();

    send(
        &mut ws,
        &Frame::new("login", serde_json::json!({"username": "ada", "password": "open sesame"})),
    )
    .await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "login.result");
    assert_eq!(f.args["user_id"].as_str(), Some(user.to_string().as_str()));
    let token = f.args["token"].as_str().expect("the login rotates the token").to_string();

    // The binding is prism's, not the booth's: the token it just minted restores the same user AND
    // the same device (an alias and a first-seen date survive with it).
    let (_ws2, ident2) = connect(addr, &format!("&token={token}")).await;
    assert_eq!(ident2["user_id"].as_str(), Some(user.to_string().as_str()));
    assert_eq!(ident2["device_id"].as_str(), Some(device.as_str()), "the same device comes back");

    // A wrong password is the booth's refusal, relayed as the standard error value.
    let (mut ws3, _) = connect(addr, "").await;
    send(&mut ws3, &Frame::new("login", serde_json::json!({"username": "ada", "password": "nope"}))).await;
    let f = recv(&mut ws3).await;
    assert_eq!(f.ev, "error");
    assert_eq!(f.args["message"].as_str(), Some("invalid credentials"));
}