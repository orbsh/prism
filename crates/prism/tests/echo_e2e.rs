//! Echo plane e2e: boot engine + gateway, register the four-language
//! echoes, drive every verb over a REAL WebSocket — JSON debug codec
//! and CBOR default both exercised. This is the acceptance map for the
//! prism Phase 0+1 landing (ADR-0017 §4 shape, reduced to echo scope).

use aura_engine::Engine;
use prism::{actors, identity::Registry, Gateway};
use prism_protocol::{Codec, Frame};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Boot one gateway + `n` echoes (feature-gated languages register via
/// the same cfgs as `actors::echo_actors`), return (addr, handles).
async fn boot() -> SocketAddr {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.into_path()).unwrap();
    let engine = Engine::start(&Default::default()).await.expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, prism::nodes::Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });
    addr
}

async fn connect(addr: SocketAddr, codec: Codec) -> WsClient {
    let qs = match codec {
        Codec::Json => "?protocol=json",
        Codec::Cbor => "",
    };
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws{qs}"))
        .await
        .expect("ws handshake");
    // The §2 connect answer arrives before anything the test asked for.
    let f = recv_frame(&mut ws, codec).await;
    assert_eq!(f.ev, "connected");
    ws
}

async fn send_frame(ws: &mut WsClient, codec: Codec, frame: &Frame) {
    let bytes = codec.encode(frame);
    ws.send(Message::Binary(bytes)).await.unwrap();
}

/// Read one frame, skipping nothing (the gateway's only chatter is the
/// dispatch answer for the request just sent — strict order).
async fn recv_frame(ws: &mut WsClient, codec: Codec) -> Frame {
    loop {
        match ws.next().await.expect("stream ended").expect("ws msg") {
            Message::Binary(b) => return codec.decode(&b).expect("frame decode"),
            Message::Text(t) => return codec.decode(t.as_bytes()).expect("frame decode"),
            Message::Close(_) => panic!("server closed"),
            _ => continue,
        }
    }
}

/// The echo round trip per language: `ev: "echo_<lang>"` (type name ==
/// handler name, the dispatch rule) — args return verbatim under
/// `echo_<lang>.result`.
async fn echo_round_trip(codec: Codec, ev: &'static str) {
    let addr = boot().await;
    let mut ws = connect(addr, codec).await;
    let payload = serde_json::json!({"msg": format!("hello {ev}"), "n": 42});
    send_frame(&mut ws, codec, &Frame::new(ev, payload.clone())).await;
    let reply = recv_frame(&mut ws, codec).await;
    assert_eq!(reply.ev, format!("{ev}.result"));
    // §7 amendment: the gateway delivers handlers a {sender, args}
    // envelope; the plain echoes pass it through whole.
    assert_eq!(reply.args["args"], payload, "{ev} echo shape");
    assert!(reply.args["sender"]["device"].as_u64().unwrap() > 0);
}

#[tokio::test]
#[cfg(feature = "steel")]
async fn steel_echo_json() {
    echo_round_trip(Codec::Json, "echo_steel").await;
}
#[tokio::test]
#[cfg(feature = "steel")]
async fn steel_echo_cbor() {
    echo_round_trip(Codec::Cbor, "echo_steel").await;
}

#[tokio::test]
#[cfg(feature = "python")]
async fn python_echo_json() {
    echo_round_trip(Codec::Json, "echo_python").await;
}
#[tokio::test]
#[cfg(feature = "python")]
async fn python_echo_cbor() {
    echo_round_trip(Codec::Cbor, "echo_python").await;
}

#[tokio::test]
#[cfg(feature = "nushell")]
async fn nushell_echo_json() {
    echo_round_trip(Codec::Json, "echo_nu").await;
}
#[tokio::test]
#[cfg(feature = "nushell")]
async fn nushell_echo_cbor() {
    echo_round_trip(Codec::Cbor, "echo_nu").await;
}

#[tokio::test]
#[cfg(feature = "wasmtime")]
async fn wasm_echo_json() {
    echo_round_trip(Codec::Json, "echo_wasm").await;
}
#[tokio::test]
#[cfg(feature = "wasmtime")]
async fn wasm_echo_cbor() {
    echo_round_trip(Codec::Cbor, "echo_wasm").await;
}

/// Fan-out: two connections, one broadcasts, BOTH receive the frame
/// (the connection-table traversal shape of ADR-0017 §3 minus auth).
#[tokio::test]
async fn broadcast_reaches_every_connection() {
    let addr = boot().await;
    let mut a = connect(addr, Codec::Json).await;
    let mut b = connect(addr, Codec::Json).await;

    send_frame(&mut a, Codec::Json, &Frame::new("broadcast", serde_json::json!({"hi": "all"})))
        .await;
    let fa = recv_frame(&mut a, Codec::Json).await;
    let fb = recv_frame(&mut b, Codec::Json).await;
    for f in [fa, fb] {
        assert_eq!(f.ev, "broadcast");
        assert_eq!(f.args, serde_json::json!({"hi": "all"}));
    }
}

/// Unknown actor → error VALUE on the same connection, socket stays
/// alive (the wire never closes for a business failure).
#[tokio::test]
async fn unknown_type_is_error_value_not_close() {
    let addr = boot().await;
    let mut ws = connect(addr, Codec::Json).await;
    send_frame(&mut ws, Codec::Json, &Frame::new("nope", serde_json::json!({}))).await;
    let f = recv_frame(&mut ws, Codec::Json).await;
    assert_eq!(f.ev, "error");
    assert_eq!(f.args["ev"], "nope");
    // still alive: ping round-trips
    send_frame(&mut ws, Codec::Json, &Frame::new("ping", serde_json::Value::Null)).await;
    let f = recv_frame(&mut ws, Codec::Json).await;
    assert_eq!(f.ev, "ping.result");
}

/// Malformed frame → error value with the decode message, connection
/// survives.
#[tokio::test]
async fn garbage_is_error_value_not_close() {
    let addr = boot().await;
    let mut ws = connect(addr, Codec::Json).await;
    ws.send(Message::Binary(b"not json".to_vec())).await.unwrap();
    let f = recv_frame(&mut ws, Codec::Json).await;
    assert_eq!(f.ev, "error");
    assert!(f.args["message"].as_str().unwrap().contains("protocol error"));
}

/// The echo sources are the documented ones (drift guard: if aura's
/// carrier shapes change, THIS test fails before the server misleads).
#[test]
fn echo_sources_match_carrier_shapes() {
    let list = actors::echo_actors();
    let has = |name: &str, lang: &str, needle: &str| {
        list.iter().any(|e| e.type_name == name && e.language == lang && e.source.contains(needle))
    };
    #[cfg(feature = "steel")]
    assert!(has("echo_steel", "steel", "(define (echo_steel"));
    #[cfg(feature = "python")]
    assert!(has("echo_python", "python", "def echo_python"));
    #[cfg(feature = "nushell")]
    assert!(has("echo_nu", "nushell", "def echo_nu"));
    #[cfg(feature = "wasmtime")]
    assert!(has("echo_wasm", "wasmtime", "AGFzbQ")); // base64 magic of \0asm
}
