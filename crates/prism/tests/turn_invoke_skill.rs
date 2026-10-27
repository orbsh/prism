//! `invoke_skill` end to end (gravity ADR-0004 §3 · ADR-0001): a turn resolves a
//! skill, the model picks one, and the step's SCRIPT runs on a real node — the
//! whole walk, through the whole stack.
//!
//! What is real here: k10r's mem-server (the skill node, its code row, its
//! doors), prism's gateway, an aura engine with a `runner_py` mount, a real
//! effector node dialled in over `/effector/<alias>`, and gravity's turn +
//! provider guests as separate processes. What is stubbed: the LLM (an SSE
//! server that plays a model which calls `invoke_skill` and then answers).
//!
//! The assertion is the MODEL's own view: round 1's request body must carry the
//! node's result as the tool message. A door that runs the script and loses the
//! answer looks exactly like a model that never asked, so the check is on the
//! body the model receives, not on a log line — and the script's value is the
//! one arithmetic fact only a real execution can produce.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use prism::{identity::Registry, mounts::MountDecl, Gateway};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;

/// ADR-0027: the node fetches its code by content reference, so the mount's
/// runner spec is served from a local HTTP face addressed by its hash.
fn serve_sources(specs: &[String]) -> u16 {
    use std::io::{Read as _, Write as _};
    let served: Vec<(String, Vec<u8>)> = specs
        .iter()
        .map(|s| {
            (
                aura_realm::meta::code_hex(&aura_realm::meta::code_hash(s)),
                s.as_bytes().to_vec(),
            )
        })
        .collect();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .trim_start_matches('/')
                .to_string();
            let body = served
                .iter()
                .find(|(hash, _)| hash == &path)
                .map(|(_, bytes)| bytes.clone())
                .unwrap_or_default();
            let status = if body.is_empty() {
                "404 Not Found"
            } else {
                "200 OK"
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    port
}

fn runner_path(file: &str) -> String {
    let path = format!(
        "{}/../../../effector/deploy/runners/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    assert!(
        std::path::Path::new(&path).exists(),
        "the runner artifact must exist at {path}"
    );
    path
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

/// The model, as a stub: round 0 asks for `invoke_skill`, round 1 REFUSES to
/// serve unless the node's answer came back in the tool message, round 2
/// answers. Bodies are recorded so the test can assert what the model saw.
async fn sse_server(skill_id: u64, produced: &'static str) -> (String, Arc<Mutex<Vec<Value>>>) {
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
                let payload = if index == 0 {
                    json!({"choices": [{"delta": {"tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": {
                            "name": "invoke_skill",
                            "arguments": json!({
                                "skill_id": skill_id,
                                "arguments": {"n": 4},
                            }).to_string(),
                        }
                    }]}}]})
                } else if index == 1 {
                    // The node ran the script; the value is in the tool message.
                    // Parsed, not substring-matched on the raw body: the tool
                    // content is a JSON document inside a JSON string, so the
                    // bytes are escaped and a naive `contains` reads a lie.
                    let saw_value = parsed["messages"]
                        .as_array()
                        .map(|messages| {
                            messages.iter().any(|message| {
                                message["role"] == "tool"
                                    && message["content"]
                                        .as_str()
                                        .map(|content| content.contains(produced))
                                        .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false);
                    if !saw_value {
                        return axum::response::Response::builder()
                            .status(500)
                            .body(axum::body::Body::from(
                                "the tool result never reached the model",
                            ))
                            .unwrap();
                    }
                    json!({"choices": [{"delta": {"content": "the sum is seven"}}]})
                } else {
                    json!({"choices": [{"delta": {"content": ""}}]})
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

// A multi-thread runtime is not a preference here: the fixture calls k10r with
// a SYNCHRONOUS client while mem-server is served by this same process, so a
// current-thread runtime would park the only worker on its own request (the
// deadlock this test first hit).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_skill_step_runs_on_the_node_and_its_value_reaches_the_model() {
    // What the step's script prints — one JSON document, an arithmetic fact no
    // stub can fake: the number comes from the payload gravity passed through.
    const PRODUCED: &str = r#"{"sum":7}"#;
    // The two guest binaries must be named BEFORE the gateway registers the
    // booth types (the spawn spec is captured at registration), so a test that
    // set them later would register "gravity-turn" as a bare PATH name.
    let gravity_bins = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../gravity/target/debug");
    std::env::set_var(
        "GRAVITY_PROVIDER_BIN",
        format!("{gravity_bins}/gravity-provider"),
    );
    std::env::set_var("GRAVITY_TURN_BIN", format!("{gravity_bins}/gravity-turn"));

    let py_spec = format!("python3 {}", runner_path("runner_py.py"));
    let http_port = serve_sources(&[py_spec.clone()]);

    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = aura_engine::Engine::start(&aura_config::EngineConfig {
        code_base_url: Some(format!("http://127.0.0.1:{http_port}")),
        ..Default::default()
    })
    .await
    .expect("engine boot");
    // The production boot path registers the echo plane AND gravity's two
    // guests (turn + provider); a test that skipped this would be testing a
    // gateway that cannot turn at all.
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine.clone(), registry, prism::nodes::Posture::Open);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gateway = tokio::spawn(async move { gw.serve(listener).await.unwrap() });

    // The mount: one type, a fixed runner, the operation language it serves.
    let decl = MountDecl {
        type_name: "runner_py".into(),
        node_alias: "skill-node".into(),
        carrier: "exec".into(),
        language: "python".into(),
        runner: py_spec.clone(),
        encoding: aura_booth::Encoding::Json,
    };
    let live = prism::mounts::register_all(&engine, std::slice::from_ref(&decl))
        .await
        .unwrap();
    assert_eq!(live, vec!["runner_py".to_string()]);

    // The node dials in (the real effector runtime, node side of this stack).
    std::env::set_var("PRISM_SKILL_E2E_CREDENTIAL", "tok");
    let node = effector_runtime::remote::run(effector_config::EffectorConfig {
        control_plane_url: format!("ws://127.0.0.1:{}/effector/skill-node", addr.port()),
        sandbox: false,
        credential_env: "PRISM_SKILL_E2E_CREDENTIAL".into(),
        capabilities: effector_config::CapabilitySurface {
            node_alias: "skill-node".into(),
            carriers: vec!["exec".into()],
            ..Default::default()
        },
    });
    let node = tokio::spawn(node);
    for _ in 0..80 {
        if engine
            .realm
            .try_lock()
            .unwrap()
            .effectors
            .contains_key("skill-node")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        engine
            .realm
            .try_lock()
            .unwrap()
            .effectors
            .contains_key("skill-node"),
        "the node registered through /effector/<alias>"
    );

    // k10r: one code row (the step's script) and one skill pointing at it.
    let store_url = mem_server().await;
    let code: Value = ureq::post(&format!("{store_url}/code"))
        .send_json(json!({
            "language": "python",
            "description": "adds three",
            "parameters": "{}",
            "source": "import json, os\nn = json.loads(os.environ['TOOLRUNNER_ARGS']).get('n', 0)\nprint(json.dumps({'sum': n + 3}))\n",
        }))
        .unwrap()
        .into_json()
        .unwrap();
    let code_id = code["code_id"].as_u64().unwrap();
    let skill: Value = ureq::post(&format!("{store_url}/skills/37"))
        .send_json(json!({
            "name": "add-three",
            "title": "adds three",
            "content": "Adds three to a number on this deployment's node.",
            "steps": json!([{ "code_id": code_id, "note": "run it" }]).to_string(),
        }))
        .unwrap()
        .into_json()
        .unwrap();
    let skill_id = skill["node_id"].as_u64().unwrap();

    // The turn's deployment facts, including the mount table: the same JSON
    // prism's own boot reads (TurnConfig::from_env → mounts), so this test
    // exercises the production path rather than a hand-made args document.
    let (endpoint, seen) = sse_server(skill_id, PRODUCED).await;
    std::env::set_var("GRAVITY_STORE_URL", &store_url);
    std::env::set_var("GRAVITY_ENDPOINT", &endpoint);
    std::env::set_var("GRAVITY_MODEL", "test-model");
    std::env::set_var("GRAVITY_KEY_ENV", "GRAVITY_TEST_KEY");
    std::env::set_var("GRAVITY_TEST_KEY", "test-key");
    std::env::set_var("GRAVITY_MEMORY_SCOPE", "37");
    std::env::set_var(
        "PRISM_MOUNTS",
        json!([{
            "type": "runner_py",
            "alias": "skill-node",
            "carrier": "exec",
            "language": "python",
            "runner": py_spec,
        }])
        .to_string(),
    );

    let mut ws = connect(addr).await;
    send(
        &mut ws,
        &prism_protocol::Frame::new(
            "signup",
            json!({ "username": "alice", "password": "pw" }),
        ),
    )
    .await;
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "signup.result");

    send(
        &mut ws,
        &prism_protocol::Frame::new(
            "turn.submit",
            json!({ "session_id": "1", "text": "add three to four" }),
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
    assert_eq!(reply, "the sum is seven");

    // What the MODEL saw: round 1 carries a tool message whose content is the
    // node's answer — the proof the walk reached execution and came back.
    let bodies = seen.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2, "one inference round before the call, one after");
    let tool_message = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("the fed-back tool message");
    assert!(
        tool_message["content"]
            .as_str()
            .unwrap_or_default()
            .contains(PRODUCED),
        "the node's value reached the model: {tool_message}"
    );

    node.abort();
    gateway.abort();
}

async fn connect(addr: SocketAddr) -> WsClient {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?protocol=json"))
        .await
        .expect("ws handshake");
    let f = recv(&mut ws).await;
    assert_eq!(f.ev, "connected");
    ws
}

type WsClient = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn send(ws: &mut WsClient, frame: &prism_protocol::Frame) {
    use futures_util::SinkExt;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        serde_json::to_string(frame).unwrap(),
    ))
    .await
    .unwrap();
}

async fn recv(ws: &mut WsClient) -> prism_protocol::Frame {
    use futures_util::StreamExt;
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