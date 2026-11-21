//! `PRISM_CODECS` — one flag, one codec (ADR-0017 §4).
//!
//! This test lives in its OWN integration-test binary on purpose: the pin is read once per
//! process (`OnceLock`), exactly as a deployment reads it once at boot, so a second value could
//! not take effect in a process that had already served a handshake. One file, one value.
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

#[tokio::test]
async fn a_pinned_deployment_refuses_the_other_codec_and_still_serves_its_own() {
    // The pin, set before the first handshake (the boot-time read). Edition 2021: `set_var` is
    // safe here, and this binary has exactly one test, so nothing races it.
    std::env::set_var("PRISM_CODECS", "json");
    let addr = boot().await;

    // The codec the deployment serves: accepted, on either rung.
    let mut ws = tokio_tungstenite::connect_async(request(addr, "?codec=json", None))
        .await
        .expect("the pinned codec is served")
        .0;
    match ws.next().await.expect("stream").expect("msg") {
        Message::Binary(bytes) => {
            let frame: Frame = Codec::Json.decode(&bytes).expect("json frame");
            assert_eq!(frame.ev, "connected");
        }
        other => panic!("expected a frame, got {other:?}"),
    }

    // The codec it does not serve: REFUSED, whichever rung asked. Accepting would hand the
    // client a wire the deployment does not produce, which is the failure the pin removes.
    for (query, offer) in [("", Some("accrete-cbor")), ("?codec=cbor", None)] {
        let refused = tokio_tungstenite::connect_async(request(addr, query, offer))
            .await
            .expect_err("cbor is outside the pin");
        let tokio_tungstenite::tungstenite::Error::Http(response) = refused else {
            panic!("a pinned refusal is an HTTP answer: {refused:?}");
        };
        assert_eq!(response.status().as_u16(), 400);
        let body =
            String::from_utf8_lossy(response.body().as_deref().unwrap_or_default()).to_string();
        assert!(
            body.contains("PRISM_CODECS"),
            "the refusal names the flag that refused it: {body:?}"
        );
    }
}
