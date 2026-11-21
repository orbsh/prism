//! A run that waits on a PERSON, end to end.
//!
//! The turn starts the skill; the plan's first step is a `request`, which the engine
//! cannot answer and does not pretend to. The form goes to the page addressed
//! (`<run>/<step>/<pass>/<attempt>`), the page's answer comes back over the venue as
//! `ui:run.answer`, and the run moves — WITHOUT a second turn, and without the model
//! hearing anything about it.
//!
//! What is real here: k10r's mem-server, prism's gateway, gravity's turn guest, and
//! the booth's `answer` entry. What is stubbed: the LLM (two rounds, then it stops
//! being asked — the count is part of the assertion).
//!
//! The assertions are the STORE's: what the run's document holds after each move. A
//! door that advances the wrong step, or that re-enters the model to do it, would
//! leave a different document or a third request at the stub.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use futures_util::StreamExt;
use prism::{identity::Registry, Gateway};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

/// The store's doors, called the way the venue calls them: async, with the body
/// already parsed. `reqwest::Client::new()` per call is fine for a fixture — it is
/// the connection pool, not the client, that costs anything here.
async fn post_json(url: &str, body: Value) -> Value {
    reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .expect("POST")
        .json()
        .await
        .expect("POST body")
}

async fn get_json(url: &str) -> Value {
    reqwest::Client::new()
        .get(url)
        .send()
        .await
        .expect("GET")
        .json()
        .await
        .expect("GET body")
}

async fn mem_server() -> String {
    let dir = tempfile::tempdir().unwrap();
    let app = mem_server::open(dir.path().to_path_buf()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::mem::forget(dir);
    format!("http://{addr}")
}

/// A plan that cannot run without a person: one `request`, then the exit.
const PLAN: &str = r#"{
    "schema_version": 1,
    "steps": [
        { "id": "ask", "kind": "request",
          "form": { "title": "Which way?", "fields": [
              { "name": "branch", "type": "choice", "options": ["left", "right"] } ] },
          "out_edges": [{ "to": "done" }] },
        { "id": "done", "kind": "end",
          "inputs": { "answer": "$.ask.branch" } }
    ]
}"#;

/// The model: one round that starts the run, one that says it is waiting. A THIRD
/// request would be the failure this test exists to catch — the answer is not a
/// conversation.
async fn sse_server(skill_id: u64) -> (String, Arc<Mutex<Vec<Value>>>) {
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
                seen.lock().unwrap().push(parsed);
                let payload = if index == 0 {
                    json!({"choices": [{"delta": {"tool_calls": [{
                        "id": "s1",
                        "type": "function",
                        "function": {
                            "name": "start",
                            "arguments": json!({ "skill_id": skill_id }).to_string(),
                        }
                    }]}}]})
                } else {
                    json!({"choices": [{"delta": {"content": "I need one answer from you"}}]})
                };
                let chunk = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(
                    axum::body::Bytes::from(format!("data: {payload}\n\n")),
                )])
                .chain(futures_util::stream::once(async {
                    Ok::<_, std::io::Error>(axum::body::Bytes::from("data: [DONE]\n\n"))
                }));
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from_stream(chunk))
                    .unwrap()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

/// The deployment's rows: where the store is, and which model to call.
async fn seed_turn_rows(engine: &aura_engine::Engine, store_url: &str, endpoint: &str) {
    let (store, plan) = {
        let realm = engine.realm.lock().await;
        (
            realm.mq.clone(),
            realm
                .plan_of(gravity_booth::TURN_TYPE)
                .cloned()
                .expect("the turn declares `settings` + `models`"),
        )
    };
    let put = |collection: &str, key: Value, doc: Value| {
        aura_realm::store_exec::execute(
            &store,
            &plan,
            &aura_booth::StoreOp {
                collection: collection.to_string(),
                op: aura_booth::StoreOpKind::PutDocument { key, doc },
            },
        )
        .expect("seed")
    };
    put(
        "models",
        json!({ "kind": 2, "id": 1 }),
        json!({
            "alias": "default",
            "endpoint": endpoint,
            "model": "test-model",
            "key_env": "GRAVITY_TEST_KEY",
            "weight": 1,
            "enabled": 1,
        }),
    );
    put(
        "settings",
        json!({ "kind": 1, "id": 1 }),
        json!({
            "store_url": store_url,
            "memory_scope": 37,
            "memory_format": "plaintext",
            "default_model": 1,
        }),
    );
}

/// The run's document, decoded. Bounded callers poll it; nothing here waits.
async fn read_run(store_url: &str, run_id: u64) -> Value {
    let body = get_json(&format!("{store_url}/runs/37/{run_id}")).await;
    serde_json::from_str(body["doc"].as_str().expect("a document")).expect("a run document")
}

type WsClient =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(addr: SocketAddr) -> WsClient {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?protocol=json"))
        .await
        .expect("ws handshake");
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "connected");
    ws
}

async fn send(ws: &mut WsClient, frame: &prism_protocol::Frame) {
    use futures_util::SinkExt;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::to_string(frame).unwrap(),
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

#[tokio::test]
async fn an_answer_moves_the_run_and_never_reaches_the_model() {
    // The guest binaries must be named BEFORE the gateway registers the booth types.
    let gravity_bins = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../gravity/target/debug");
    std::env::set_var(
        "GRAVITY_PROVIDER_BIN",
        format!("{gravity_bins}/gravity-provider"),
    );
    std::env::set_var("GRAVITY_TURN_BIN", format!("{gravity_bins}/gravity-turn"));
    std::env::set_var("GRAVITY_TEST_KEY", "test-key");

    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = aura_engine::Engine::start(&aura_config::EngineConfig::default())
        .await
        .expect("engine boot");
    Gateway::with_auth(&engine, &registry).await.unwrap();
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine.clone(), registry, prism::nodes::Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });

    let store_url = mem_server().await;
    // The skill: a plan with a step only a person can answer.
    let skill: Value = post_json(
        &format!("{store_url}/skills/37"),
        json!({
            "name": "ask-the-user",
            "title": "asks",
            "content": "Asks which way to go, then finishes.",
            "steps": PLAN,
        }),
    )
    .await;
    let skill_id = skill["node_id"].as_u64().unwrap();

    let (endpoint, seen) = sse_server(skill_id).await;
    seed_turn_rows(&engine, &store_url, &endpoint).await;

    let mut ws = connect(addr).await;
    send(
        &mut ws,
        &prism_protocol::Frame::new("signup", json!({ "login_name": "alice", "password": "pw" })),
    )
    .await;
    assert_eq!(recv(&mut ws).await.ev, "signup.result");

    send(
        &mut ws,
        &prism_protocol::Frame::new(
            "ui:turn.submit",
            // NO `member_id`: a page does not name its speaker, the venue stamps it. (A number in
            // that field is what this fixture used to send, and k10r refuses it — member ids are
            // uuids — which is a usage error surfacing as a turn failure.)
            json!({
                "channel_id": 7,
                "user_message": "which way?",
                "event": "ui:turn.delta",
                "mounts": [],
            }),
        ),
    )
    .await;
    let mut reply = String::new();
    loop {
        let f = recv(&mut ws).await;
        match f.ev.as_str() {
            "turn.delta" => reply.push_str(f.args["delta"].as_str().unwrap_or_default()),
            "turn.done" => break,
            "turn.error" => panic!("the turn failed: {}", f.args),
            _ => {}
        }
    }
    assert_eq!(reply, "I need one answer from you");

    // The run exists, is rooted at the skill, and has NOTHING in its log: the ask has
    // not been answered, so nothing has happened.
    let listed: Value = get_json(&format!("{store_url}/runs/37")).await;
    let runs = listed.as_array().expect("a list");
    assert_eq!(runs.len(), 1, "one run: {listed}");
    let run_id = runs[0]["run_id"].as_u64().unwrap();
    let doc: Value = get_json(&format!("{store_url}/runs/37/{run_id}")).await;
    let text = doc["doc"].as_str().unwrap();
    assert!(
        text.contains(&format!("\"skill:37/{skill_id}\"")),
        "the run says which skill it is: {text}"
    );
    let run: Value = serde_json::from_str(text).unwrap();
    // An empty log is OMITTED from the document (no `entries` key at all).
    assert_eq!(run["entries"].as_array().map_or(0, Vec::len), 0, "{run}");

    // THE ANSWER. Addressed by the form's own id; the model is not in this at all. The payload is
    // the FIELD MAP a form's uplink carries — `{field: {data, payload}}`, where `data` is what the
    // person chose — because the venue wraps a caller's args under `data` and the booth unwraps
    // that one wrapper, not a second one (a plan reads `$.ask.branch`, never `$.ask.branch.data`).
    send(
        &mut ws,
        &prism_protocol::Frame::new(
            "ui:run.answer",
            json!({ "id": format!("{run_id}/ask/0/0"), "branch": { "data": "left" } }),
        ),
    )
    .await;
    // A PAGE DOES NOT HEAR ITS OWN INPUT ECHOED (prism ui.rs excludes the sender), so
    // the answering connection is not where the consequence shows up — the STORE is.
    // Reads are bounded: a door that never answers must fail the test, not hang it.
    let mut moved = read_run(&store_url, run_id).await;
    for _ in 0..50 {
        if moved["entries"].as_array().map(Vec::len).unwrap_or(0) >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        moved = read_run(&store_url, run_id).await;
    }

    let run = moved;
    let entries = run["entries"].as_array().unwrap();
    let steps: Vec<&str> = entries
        .iter()
        .map(|e| e["step"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        steps,
        vec!["ask", "done"],
        "the answer, then the exit: {run}"
    );
    assert_eq!(entries[0]["output"]["branch"], "left", "{run}");

    // And the model was asked TWICE — the answer added no round.
    assert_eq!(seen.lock().unwrap().len(), 2);
}
