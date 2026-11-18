//! The grant surface as an operator drives it: declare, LIST, read, WITHDRAW.
//!
//! A management face that can only be read one kind at a time is one you cannot operate from — the
//! first question is "what is declared here", and answering it by guessing the kinds is not an
//! answer. Withdrawal is the other half: a grant nobody can take away is a permission that outlives
//! the reason it was given.
//!
//! What this test insists on beyond the status codes: the listing NEVER carries a credential value.
//! A grant names its key (ns 38 is a separate table for exactly this reason), so an admin surface
//! that echoed the secret would undo the separation the storage model paid for.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use aura_engine::Engine;
use prism::{identity::Registry, nodes::Posture, Gateway};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn boot() -> std::net::SocketAddr {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });
    addr
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
    let body_part = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, serde_json::from_str(body_part).unwrap_or(serde_json::Value::Null))
}

/// The same request, keeping the body as text: refusals are sentences, not JSON.
async fn raw(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: serde_json::Value,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
        body.to_string().len()
    );
    s.write_all(head.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let resp = String::from_utf8_lossy(&buf).into_owned();
    let status: u16 = resp[9..12].parse().unwrap();
    let text = resp
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, text)
}

#[tokio::test]
async fn grants_are_listed_read_and_withdrawn() {
    let addr = boot().await;

    let (_status, body) = request(addr, "GET", "/admin/resources", None).await;
    assert_eq!(_status, 200, "{body}");
    assert_eq!(body, json!([]), "nothing declared yet, and the listing says so");

    let (status, body) = request(
        addr,
        "POST",
        "/admin/resources",
        Some(json!({
            "kind": "git",
            "base": "ssh://git.example/team",
            "mode": "rw",
            "desc": "the team's repositories",
            "secret": { "name": "GIT_TOKEN", "value": "the-credential" }
        })),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["base"], "ssh://git.example/team");
    assert_eq!(body["mode"], 1, "rw is stored as the number the row indexes on");

    let (status, body) = request(addr, "GET", "/admin/resources", None).await;
    assert_eq!(status, 200, "{body}");
    let rows = body.as_array().expect("a listing is an array");
    assert_eq!(rows.len(), 1, "one row per kind: {body}");
    assert_eq!(rows[0]["kind"], "git");
    assert_eq!(rows[0]["grant"]["base"], "ssh://git.example/team");
    let rendered = rows[0].to_string();
    assert!(
        !rendered.contains("the-credential"),
        "the listing must never carry the credential — a grant names its key: {rendered}"
    );

    let (status, body) = request(addr, "GET", "/admin/resources/git", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["secret_name"], "GIT_TOKEN");

    let (status, body) = request(addr, "DELETE", "/admin/resources/git", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["revoked"], "git");

    let (_, body) = request(addr, "GET", "/admin/resources", None).await;
    assert_eq!(body, json!([]), "withdrawn means gone from the listing: {body}");
    let (status, _) = request(addr, "GET", "/admin/resources/git", None).await;
    assert_eq!(status, 404, "and unreadable by name");
    let (status, _) = request(addr, "DELETE", "/admin/resources/git", None).await;
    assert_eq!(
        status, 404,
        "withdrawing what is not there is not a success: silence would hide a typo"
    );
}

/// A declaration the venue cannot serve is refused at the door, by name.
///
/// `archive` is a word in the resource protocol — a node refuses it BY NAME — and that is exactly why
/// it must not be declared: the operator would get a 201, a listing entry, and a task that fails
/// later saying the node cannot move it. The check runs whether or not a credential comes with the
/// declaration; the empty-secret path is the door it used to slip through.
#[tokio::test]
async fn a_kind_no_node_can_move_is_refused_at_the_door() {
    let addr = boot().await;

    for kind in ["archive", "ftp"] {
        let (status, text) = raw(
            addr,
            "POST",
            "/admin/resources",
            json!({
                "kind": kind,
                "base": "ssh://git.example/team",
                "mode": "ro",
                "secret": { "name": "K", "value": "v" }
            }),
        )
        .await;
        assert_eq!(status, 400, "{text}");
        assert!(text.contains(kind), "the refusal names the kind: {text}");

        // No credential: the same refusal, not a pass.
        let (status, text) = raw(
            addr,
            "POST",
            "/admin/resources",
            json!({ "kind": kind, "base": "ssh://git.example/team", "mode": "ro" }),
        )
        .await;
        assert_eq!(status, 400, "{text}");
        assert!(text.contains(kind), "the refusal names the kind: {text}");
    }

    let (status, body) = request(addr, "GET", "/admin/resources", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!([]),
        "a refused declaration leaves no row and no credential: {body}"
    );
}