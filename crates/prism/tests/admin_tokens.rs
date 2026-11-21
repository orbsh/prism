//! Account tokens for unattended callers: `POST /admin/tokens` is login over HTTP.
//!
//! A script cannot keep a socket open for a login and should not carry a password, so this route
//! takes the password once and hands back the same thing a login hands a page — a device bound to
//! the account, and the token that proves it. The password is the whole authorization: without it
//! the answer is 401 and nothing is minted, which is what keeps this from being an operator power.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use aura_engine::Engine;
use prism::{identity::Registry, nodes::Posture, Gateway};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A venue with one account in it, created the way a person creates one — the `signup` verb on the
/// wire, not a row poked into the store — so what this file tests is the real credential path.
async fn boot_with_account(login: &str, password: &str) -> (std::net::SocketAddr, String) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_auth(&engine, &registry).await.unwrap();
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });

    use futures_util::SinkExt;
    let url = format!("ws://{addr}/ws?protocol=json");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("ws handshake");
    assert_eq!(next(&mut ws).await.ev, "connected");
    let signup = prism_protocol::Frame::new(
        "signup",
        json!({ "login_name": login, "password": password }),
    );
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::to_string(&signup).unwrap().into(),
    ))
    .await
    .unwrap();
    let answer = next(&mut ws).await;
    assert_eq!(answer.ev, "signup.result", "{}", answer.args);
    let user = answer.args["user_id"].as_str().expect("a user id").to_string();
    (addr, user)
}

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// One frame off the wire, skipping control messages.
async fn next(ws: &mut WsClient) -> prism_protocol::Frame {
    use futures_util::StreamExt;
    loop {
        match ws.next().await.expect("stream ended").expect("ws msg") {
            tokio_tungstenite::tungstenite::Message::Binary(b) => {
                return prism_protocol::Codec::Json.decode(&b).unwrap()
            }
            tokio_tungstenite::tungstenite::Message::Text(t) => {
                return prism_protocol::Codec::Json.decode(t.as_bytes()).unwrap()
            }
            _ => continue,
        }
    }
}

async fn request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let head = match &body {
        Some(b) => format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{b}",
            b.to_string().len()
        ),
        None => format!("{method} {path} HTTP/1.1\r\nHost: x\r\n\r\n"),
    };
    s.write_all(head.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let resp = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = resp[9..12].parse().unwrap();
    let body = resp
        .split_once("\r\n\r\n")
        .map(|(_, body)| serde_json::from_str(body).unwrap_or(serde_json::Value::Null))
        .unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn a_password_buys_a_token_and_that_token_is_the_account() {
    let (addr, user) = boot_with_account("ci", "tok3n").await;

    let (status, body) = request(
        addr,
        "POST",
        "/admin/tokens",
        Some(json!({ "login_name": "ci", "password": "tok3n", "alias": "ci" })),
    )
    .await;
    assert_eq!(status, 201, "minted: {body}");
    assert_eq!(body["user_id"], json!(user), "the account it proves: {body}");
    assert_eq!(
        body["device_id"].as_str().unwrap().len(),
        36,
        "a device of its own, so the caller is not one device with the person's pages: {body}"
    );
    let token = body["token"].as_str().expect("a token").to_string();

    // The token is the credential: presenting it on the wire restores the binding, which is exactly
    // what a login does — that is why this route is login over HTTP and not a second identity.
    let url = format!("ws://{addr}/ws?protocol=json&token={token}");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("ws handshake");
    let frame = next(&mut ws).await;
    assert_eq!(frame.ev, "connected");
    assert_eq!(
        frame.args["user_id"],
        json!(user),
        "the token is the account: {}",
        frame.args
    );
}

#[tokio::test]
async fn no_password_no_token() {
    let (addr, _user) = boot_with_account("ci", "tok3n").await;

    let (status, body) = request(
        addr,
        "POST",
        "/admin/tokens",
        Some(json!({ "login_name": "ci", "password": "wrong" })),
    )
    .await;
    assert_eq!(status, 401, "wrong password: {body}");
    assert!(
        body.get("token").is_none(),
        "and nothing was minted: {body}"
    );

    // An unknown account is the SAME answer: which half was wrong is not the caller's business.
    let (status, _) = request(
        addr,
        "POST",
        "/admin/tokens",
        Some(json!({ "login_name": "nobody", "password": "wrong" })),
    )
    .await;
    assert_eq!(status, 401);
}