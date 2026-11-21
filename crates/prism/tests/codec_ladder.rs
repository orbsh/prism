//! THE HANDSHAKE LADDER over real sockets (ADR-0017 §4): every rung prism accepts, and the
//! refusals that are the whole point of not falling through.
//!
//! A refusal is checked as the TRANSPORT sees it — `Error::Http(400)` with the reason in the
//! body — because "the client learns here" is the property the ladder exists for; a test that
//! only asserted our own function's `Result` would pass with the venue closing the socket
//! silently instead.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use aura_engine::Engine;
use futures_util::StreamExt;
use prism::{identity::Registry, nodes::Posture, Gateway};
use prism_protocol::{Codec, Frame};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

async fn boot() -> SocketAddr {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = Engine::start(&Default::default())
        .await
        .expect("engine boot");
    let gw = Gateway::new(engine, registry, Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });
    addr
}

/// One client handshake: the query a client may write, and the subprotocol list it may offer.
fn request(addr: SocketAddr, query: &str, offer: Option<&str>) -> Request {
    let mut request = format!("ws://{addr}/ws{query}")
        .into_client_request()
        .expect("a ws:// request");
    if let Some(list) = offer {
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(list).expect("header value"),
        );
    }
    request
}

/// The `connected` frame every accept answers with, decoded with the codec the connection
/// negotiated — which is what proves the negotiated codec is the one the socket is using.
async fn first_frame(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    codec: Codec,
) -> Frame {
    loop {
        match ws.next().await.expect("stream").expect("msg") {
            Message::Binary(bytes) => return codec.decode(&bytes).expect("frame"),
            _ => continue,
        }
    }
}

#[tokio::test]
async fn a_subprotocol_offer_is_accepted_echoed_and_the_codec_is_in_force() {
    let addr = boot().await;
    // The client lists ours second, after a foreign protocol: the foreign one is somebody
    // else's and is skipped, ours is taken and echoed back. Written without a space because
    // tungstenite's CLIENT compares the echoed value against its own split (untrimmed); the
    // server trims, so a browser's `chat, accrete-cbor` parses identically there.
    let (mut ws, response) =
        tokio_tungstenite::connect_async(request(addr, "", Some("chat,accrete-cbor")))
            .await
            .expect("the offer is accepted");
    assert_eq!(response.status().as_u16(), 101);
    assert_eq!(
        response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok()),
        Some("accrete-cbor"),
        "the chosen subprotocol is ECHOED — the client reads its answer here"
    );
    let frame = first_frame(&mut ws, Codec::Cbor).await;
    assert_eq!(frame.ev, "connected");
}

#[tokio::test]
async fn the_query_rungs_work_and_nothing_said_is_json() {
    let addr = boot().await;

    // `?codec=` — the renderer's spelling.
    let (mut ws, response) = tokio_tungstenite::connect_async(request(addr, "?codec=cbor", None))
        .await
        .expect("?codec=cbor");
    assert!(
        response.headers().get("sec-websocket-protocol").is_none(),
        "no subprotocol was offered, so none is echoed"
    );
    assert_eq!(first_frame(&mut ws, Codec::Cbor).await.ev, "connected");

    // The legacy `?protocol=` — kept because clients already send it.
    let mut ws = tokio_tungstenite::connect_async(request(addr, "?protocol=json", None))
        .await
        .expect("?protocol=json")
        .0;
    assert_eq!(first_frame(&mut ws, Codec::Json).await.ev, "connected");

    // NOTHING SAID: json, the debuggable one (the ladder's last rung — not the old CBOR
    // default, which a client could not have discovered).
    let mut ws = tokio_tungstenite::connect_async(request(addr, "", None))
        .await
        .expect("no query")
        .0;
    assert_eq!(first_frame(&mut ws, Codec::Json).await.ev, "connected");
}

#[tokio::test]
async fn an_accrete_word_we_do_not_speak_is_refused_4xx() {
    let addr = boot().await;
    let refused = tokio_tungstenite::connect_async(request(addr, "", Some("accrete-msgpack")))
        .await
        .expect_err("`accrete-msgpack` is our namespace and not ours to serve");
    let tokio_tungstenite::tungstenite::Error::Http(response) = refused else {
        panic!("a refusal is an HTTP answer, not a dropped socket: {refused:?}");
    };
    assert_eq!(response.status().as_u16(), 400);
    let body = String::from_utf8_lossy(response.body().as_deref().unwrap_or_default()).to_string();
    assert!(
        body.contains("accrete-msgpack"),
        "the reason names the value the client wrote: {body:?}"
    );
}

#[tokio::test]
async fn a_bad_codec_query_is_refused_4xx_rather_than_downgraded() {
    let addr = boot().await;
    for query in ["?codec=msgpack", "?protocol=binary"] {
        let refused = tokio_tungstenite::connect_async(request(addr, query, None))
            .await
            .expect_err(query);
        let tokio_tungstenite::tungstenite::Error::Http(response) = refused else {
            panic!("{query} must be an HTTP refusal: {refused:?}");
        };
        assert_eq!(response.status().as_u16(), 400, "{query}");
    }
}
