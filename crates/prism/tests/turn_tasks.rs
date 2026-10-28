//! A task opened in one turn is the next turn's starting point (k10r ADR-0021 ·
//! gravity ADR-0004's task group): the whole loop through the whole stack.
//!
//! What is real here: k10r's mem-server (the task doors and the ns 44 sheet),
//! prism's gateway, an aura engine, and gravity's turn + provider guests as
//! separate processes. What is stubbed: the LLM — an SSE server that plays a model
//! which calls `open_task` once, then REFUSES to answer any later round unless the
//! request it was handed already carries the task.
//!
//! The assertion is deliberately on the MODEL's view, not on a store read and not on
//! a log line. A store that holds a task nobody is told about is the exact failure
//! this feature exists to end — "every turn starts from zero" — and only the body
//! the model receives can tell the difference between "the task is stored" and "the
//! task is KNOWN".
//!
//! Two later turns are scripted, and they check different things. The second proves
//! a task named `open` reaches the prompt; the third proves one named `doing` does
//! too — the first version of the prompt asked the store for `state=open` only, so
//! moving a task to `doing` dropped it out of view and the work actually in flight
//! became the one kind of work the agent could not see. The third turn is the test
//! that would have caught it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use futures_util::{SinkExt, StreamExt};
use prism::{identity::Registry, Gateway};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

type WsClient = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

const TASK: &str = "ship-the-face-change";
const NEXT: &str = "read the diff before the split";

/// k10r's own HTTP face, in-process (the same fixture `turn_invoke_skill` uses:
/// the store is a library, so a real one costs a temp dir).
async fn mem_server() -> String {
    let dir = tempfile::tempdir().unwrap();
    let app = mem_server::open(dir.path().to_path_buf()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::mem::forget(dir);
    format!("http://{addr}")
}

/// Does any SYSTEM message carry this text? The task block rides a system message
/// beside the store's rendered memory, so this is where a client-side promise would
/// have to show up to be worth anything.
fn saw_system(parsed: &Value, needle: &str) -> bool {
    parsed["messages"]
        .as_array()
        .map(|messages| {
            messages.iter().any(|message| {
                message["role"] == "system"
                    && message["content"]
                        .as_str()
                        .map(|content| content.contains(needle))
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// The model, as a stub: round 0 opens a task and stops; every later round is a new
/// turn, and each one refuses to answer unless the prompt it was given already
/// knows about the work.
async fn sse_server() -> (String, Arc<Mutex<Vec<Value>>>) {
    use axum::routing::post;
    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let rounds = Arc::new(Mutex::new(0usize));
    let recorder = seen.clone();
    let app = axum::Router::new().route(
        "/chat/completions",
        post(move |body: String| {
            let seen = recorder.clone();
            let rounds = rounds.clone();
            async move {
                let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let index = {
                    let mut n = rounds.lock().unwrap();
                    let index = *n;
                    *n += 1;
                    index
                };
                seen.lock().unwrap().push(parsed.clone());

                let open = json!({"choices": [{"delta": {"tool_calls": [{
                    "id": "c0",
                    "type": "function",
                    "function": {
                        "name": "open_task",
                        "arguments": json!({ "name": TASK, "next": NEXT }).to_string(),
                    }
                }]}}]});
                let say = |text: &str| json!({"choices": [{"delta": {"content": text}}]});
                let refusal = |why: &str| {
                    axum::response::Response::builder()
                        .status(500)
                        .body(axum::body::Body::from(why.to_string()))
                        .unwrap()
                };

                let payload = if index == 0 {
                    open
                } else if index == 1 {
                    // The round after the tool call: the door's answer came back,
                    // so the turn may end.
                    say("opened it")
                } else if index == 2 {
                    // Turn 2, first round: the task must already be in view.
                    if !saw_system(&parsed, TASK) || !saw_system(&parsed, "open") {
                        return refusal("turn 2's prompt never carried the open task");
                    }
                    if !saw_system(&parsed, NEXT) {
                        return refusal("turn 2's prompt lost the next action");
                    }
                    say("still open")
                } else if index == 3 {
                    // Turn 3, first round: the task moved to `doing` in between.
                    if !saw_system(&parsed, "doing") {
                        return refusal("turn 3's prompt dropped the work in progress");
                    }
                    say("in flight")
                } else {
                    say("")
                };

                let chunks = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(
                    axum::body::Bytes::from(format!("data: {payload}\n\n")),
                )])
                .chain(futures_util::stream::once(async {
                    Ok::<_, std::io::Error>(axum::body::Bytes::from("data: [DONE]\n\n"))
                }));
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
    (format!("http://{addr}"), seen)
}

async fn connect(addr: SocketAddr) -> WsClient {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?protocol=json"))
        .await
        .expect("ws handshake");
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "connected");
    ws
}

async fn send(ws: &mut WsClient, frame: &prism_protocol::Frame) {
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::to_string(frame).unwrap().into(),
    ))
    .await
    .unwrap();
}

async fn recv(ws: &mut WsClient) -> prism_protocol::Frame {
    loop {
        match ws.next().await.expect("stream ended").expect("ws msg") {
            tokio_tungstenite::tungstenite::Message::Binary(b) => {
                return prism_protocol::Codec::Json.decode(&b).unwrap()
            }
            tokio_tungstenite::tungstenite::Message::Text(t) => {
                return prism_protocol::Codec::Json.decode(t.as_bytes()).unwrap()
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => panic!("server closed"),
            _ => continue,
        }
    }
}

/// Submit one turn and return the reply the model's text concatenated to. A turn
/// error is a panic carrying the text: a stub that refused tells the failure story,
/// and a test that swallowed it would pass on a stack that never reached the prompt.
async fn turn(ws: &mut WsClient, session: &str, text: &str) -> String {
    send(
        ws,
        &prism_protocol::Frame::new(
            "turn.submit",
            json!({ "session_id": session, "text": text }),
        ),
    )
    .await;
    let mut reply = String::new();
    loop {
        let f = recv(ws).await;
        match f.ev.as_str() {
            "turn.delta" => reply.push_str(f.args["delta"].as_str().unwrap_or_default()),
            "turn.done" => return reply,
            "turn.error" => panic!("the turn failed: {}", f.args),
            _ => {}
        }
    }
}

// A multi-thread runtime, for the reason `turn_invoke_skill` records: the fixture
// calls k10r SYNCHRONOUSLY while mem-server is served by this same process.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_opened_in_one_turn_is_the_next_turns_starting_point() {
    // The two guest binaries must be named BEFORE the gateway registers the booth
    // types (the spawn spec is captured at registration).
    let gravity_bins = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../gravity/target/debug"
    );
    std::env::set_var(
        "GRAVITY_PROVIDER_BIN",
        format!("{gravity_bins}/gravity-provider"),
    );
    std::env::set_var("GRAVITY_TURN_BIN", format!("{gravity_bins}/gravity-turn"));

    let store_url = mem_server().await;
    let (endpoint, seen) = sse_server().await;
    std::env::set_var("GRAVITY_STORE_URL", &store_url);
    std::env::set_var("GRAVITY_ENDPOINT", &endpoint);
    std::env::set_var("GRAVITY_MODEL", "test-model");
    std::env::set_var("GRAVITY_KEY_ENV", "GRAVITY_TEST_KEY");
    std::env::set_var("GRAVITY_TEST_KEY", "test-key");
    std::env::set_var("GRAVITY_MEMORY_SCOPE", "37");

    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = aura_engine::Engine::start(&aura_config::EngineConfig::default())
        .await
        .expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, prism::nodes::Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });

    let mut ws = connect(addr).await;
    send(
        &mut ws,
        &prism_protocol::Frame::new(
            "signup",
            json!({ "username": "alice", "password": "pw" }),
        ),
    )
    .await;
    assert_eq!(recv(&mut ws).await.ev, "signup.result");

    // Turn 1: the model opens a task through its own door.
    assert_eq!(
        turn(&mut ws, "1", "remember this work: ship the face change").await,
        "opened it"
    );

    // The door really wrote it — a vertex in the fact graph plus its sheet.
    let open: Value = ureq::get(&format!("{store_url}/tasks/37?state=open"))
        .call()
        .expect("the task door answers")
        .into_json()
        .unwrap();
    let tasks = open.as_array().expect("a list of tasks");
    assert_eq!(tasks.len(), 1, "one task was opened: {open}");
    assert_eq!(tasks[0]["name"], TASK);
    assert_eq!(tasks[0]["next"], NEXT);
    assert_eq!(tasks[0]["state"], "open");
    let id = tasks[0]["node_id"].as_u64().expect("a node id");

    // Turn 2: a NEW turn in the same session. Nothing was said in it about the
    // task, so the only way the stub can see it is k10r's list reaching the prompt.
    assert_eq!(turn(&mut ws, "1", "what am I working on?").await, "still open");

    // Turn 3: the task moves to `doing` between turns, which is what a model does
    // when it starts the work — and the prompt must still carry it.
    ureq::post(&format!("{store_url}/tasks/37/{id}/move"))
        .send_json(json!({ "state": "doing" }))
        .expect("the move door answers");
    assert_eq!(turn(&mut ws, "1", "keep going").await, "in flight");

    // And the reason to look at the bodies at all: five rounds, each one a real
    // inference request from a real turn process.
    let bodies = seen.lock().unwrap().clone();
    assert_eq!(bodies.len(), 4, "two rounds for turn 1, one each for turns 2 and 3");
    assert!(
        !saw_system(&bodies[0], TASK),
        "turn 1 opened the task, so it could not have been in its own prompt"
    );
}