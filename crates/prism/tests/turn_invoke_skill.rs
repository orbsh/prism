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

/// THE DEPLOYMENT FACTS ARE ROWS (gravity ADR-0007): the store URL, the memory scope and the model
/// recipe live in the turn type's own namespace, so a venue keeps no model vocabulary and this test
/// writes what a deployment's management face writes. The type must be REGISTERED first —
/// registration persists the schema and allocates the ns, which is also what proves the guest's
/// `storage` block reaches the realm as a plan.
async fn seed_turn_rows(engine: &aura_engine::Engine, store_url: &str, endpoint: &str) {
    let (store, plan) = {
        // AWAIT the realm, never `try_lock` it: the engine's boot tasks hold it for a moment.
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

/// The model, as a stub. Round 0 walks step 0; round 1 REFUSES to serve unless
/// step 0's value came back, then walks step 1; round 2 REFUSES unless step 1's
/// value came back, then answers. Two different code rows in one turn is also
/// what makes the turn a COMPOSITION the walk must report (k10r ADR-0016), so
/// this stub drives both the execution and the write side.
async fn sse_server(
    skill_id: u64,
    first: &'static str,
    second: &'static str,
) -> (String, Arc<Mutex<Vec<Value>>>) {
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

                // Did a tool message carry this exact document? Parsed, not
                // substring-matched on the raw body: the tool content is a JSON
                // document inside a JSON string, so the bytes are escaped and a
                // naive `contains` reads a lie.
                let saw = |needle: &str| {
                    parsed["messages"]
                        .as_array()
                        .map(|messages| {
                            messages.iter().any(|message| {
                                message["role"] == "tool"
                                    && message["content"]
                                        .as_str()
                                        .map(|content| content.contains(needle))
                                        .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                };
                let invoke = |step: u64| {
                    json!({"choices": [{"delta": {"tool_calls": [{
                        "id": format!("c{step}"),
                        "type": "function",
                        "function": {
                            "name": "invoke_skill",
                            "arguments": json!({
                                "skill_id": skill_id,
                                "step": step,
                                "arguments": {"n": 4},
                            }).to_string(),
                        }
                    }]}}]})
                };

                let refusal = |why: &str| {
                    axum::response::Response::builder()
                        .status(500)
                        .body(axum::body::Body::from(why.to_string()))
                        .unwrap()
                };
                let payload = if index == 0 {
                    invoke(0)
                } else if index == 1 {
                    if !saw(first) {
                        return refusal("step 0's result never reached the model");
                    }
                    invoke(1)
                } else if index == 2 {
                    if !saw(second) {
                        return refusal("step 1's result never reached the model");
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
    // The second step's own document: a DIFFERENT code row, which is what makes
    // this turn a composition rather than a retry (k10r ADR-0016).
    const PRODUCED_2: &str = r#"{"total":9}"#;
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

    // The node dials in (the real effector runtime, node side of this stack) with a key file:
    // the mount verifies a signature over its per-connection nonce (effector ADR-0002).
    let mut key_path = std::env::temp_dir();
    key_path.push(format!("prism-skill-e2e-{}.key", std::process::id()));
    let _ = std::fs::remove_file(&key_path);
    effector_protocol::write_keypair(&key_path).unwrap();
    let node = effector_runtime::remote::run(effector_config::EffectorConfig {
        control_plane_url: format!("ws://127.0.0.1:{}/effector/skill-node", addr.port()),
        sandbox: false,
        private_key_path: key_path.display().to_string(),
        capabilities: effector_config::CapabilitySurface {
            node_alias: "skill-node".into(),
            carriers: vec!["exec".into()],
            ..Default::default()
        },
    });
    let node = tokio::spawn(node);
    // `.lock().await`, not `try_lock().unwrap()`: the polling loop runs while the
    // node task is starting up and holding the realm, so a try-lock here fails on
    // scheduling rather than on the thing being tested. Waiting is what the loop
    // means.
    for _ in 0..80 {
        if engine
            .realm
            .lock()
            .await
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
            .lock()
            .await
            .effectors
            .contains_key("skill-node"),
        "the node registered through /effector/<alias>"
    );

    // k10r: two code rows (one per step) and one skill pointing at both, in
    // order — the composition a turn can then report.
    let store_url = mem_server().await;
    let mut code_ids = Vec::new();
    for (description, source) in [
        (
            "adds three",
            "import json, os\nn = json.loads(os.environ['TOOLRUNNER_ARGS']).get('n', 0)\nprint(json.dumps({'sum': n + 3}))\n",
        ),
        (
            "doubles and adds one",
            "import json, os\nn = json.loads(os.environ['TOOLRUNNER_ARGS']).get('n', 0)\nprint(json.dumps({'total': n * 2 + 1}))\n",
        ),
    ] {
        let code: Value = ureq::post(&format!("{store_url}/code"))
            .send_json(json!({
                "language": "python",
                "description": description,
                "parameters": "{}",
                "source": source,
            }))
            .unwrap()
            .into_json()
            .unwrap();
        code_ids.push(code["code_id"].as_u64().unwrap());
    }
    let skill: Value = ureq::post(&format!("{store_url}/skills/37"))
        .send_json(json!({
            "name": "add-three",
            "title": "adds three",
            "content": "Adds three to a number on this deployment's node.",
            "steps": json!([
                { "code_id": code_ids[0], "note": "adds three" },
                { "code_id": code_ids[1], "note": "doubles and adds one" },
            ])
            .to_string(),
        }))
        .unwrap()
        .into_json()
        .unwrap();
    let skill_id = skill["node_id"].as_u64().unwrap();

    // The turn's deployment facts are ROWS in its own namespace now (gravity ADR-0007); only the
    // two spawn specs are environment, because they are where the binary lives.
    let (endpoint, seen) = sse_server(skill_id, PRODUCED, PRODUCED_2).await;
    seed_turn_rows(&engine, &store_url, &endpoint).await;
    std::env::set_var("GRAVITY_TEST_KEY", "test-key");

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

    // The venue's own word for "run a turn": a page's bare `dest` becomes `ui:turn.submit` on the
    // plane, and gravity's booth declares that event. Everything the page knows rides the payload —
    // the conversation, the text, the mount table its manifest declares (ADR-0001 §7), and the
    // event its chunks are spoken as.
    send(
        &mut ws,
        &prism_protocol::Frame::new(
            "ui:turn.submit",
            json!({
                "channel_id": 7,
                "member_id": 1,
                "user_message": "add three to four",
                "event": "ui:turn.delta",
                "mounts": [{"language": "python", "type": "runner_py"}],
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
    assert_eq!(reply, "the sum is seven");

    // What the MODEL saw: round 1 carries a tool message whose content is the
    // node's answer — the proof the walk reached execution and came back.
    let bodies = seen.lock().unwrap().clone();
    assert_eq!(
        bodies.len(),
        3,
        "one round to start, one per step, one to answer"
    );
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

    // The write side of the loop (k10r ADR-0016): the turn walked two DIFFERENT
    // code rows, so it reported the composition — counted, NOT promoted. The
    // key is the order the rows ran in, spelled by the walker.
    let proposals: Value = ureq::get(&format!("{store_url}/skills/37/proposals"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    let rows = proposals.as_array().unwrap();
    assert_eq!(rows.len(), 1, "one composition: {proposals}");
    assert_eq!(
        rows[0]["key"],
        format!("{}+{}", code_ids[0], code_ids[1]),
        "the ids in execution order"
    );
    assert_eq!(rows[0]["count"], 1, "seen once");
    assert_eq!(rows[0]["promoted_to"], "", "nobody has promoted it");
    let steps = rows[0]["steps"].as_str().unwrap();
    assert!(
        steps.contains(&code_ids[0].to_string()) && steps.contains(&code_ids[1].to_string()),
        "the steps name what ran: {steps}"
    );
    assert!(
        steps.contains("adds three"),
        "and the descriptions a reviewer reads come from the code rows: {steps}"
    );

    // Both steps were counted ON THE NODE (k10r ADR-0018): an attempt before
    // each call and a success after it. Two steps of one skill = two calls, two
    // successes — and the counters are the node's own DECLARED FIELDS (the
    // 2026-10-27 amendment: a number every call bumps is not dynamic data), not
    // the weight system's edge counters.
    let skill_row: Value = ureq::get(&format!("{store_url}/skills/37/{skill_id}"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    let raw: Value = ureq::get(&format!("{store_url}/graph/37"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    let skill_node = raw["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| candidate["id"].as_u64() == Some(skill_id))
        .expect("the skill node is in the graph");
    assert_eq!(skill_node["name"], skill_row["name"]);
    assert_eq!(
        skill_node["invoke_calls"].as_u64(),
        Some(2),
        "two steps walked, two attempts counted: {skill_node}"
    );
    assert_eq!(
        skill_node["invoke_ok"].as_u64(),
        Some(2),
        "and both came back"
    );

    // Nothing promoted itself: the resolver still answers only the real skill.
    let resolved: Value = ureq::post(&format!("{store_url}/skills/37/resolve"))
        .send_json(json!({ "query": "add three" }))
        .unwrap()
        .into_json()
        .unwrap();
    let names: Vec<&str> = resolved
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["add-three"], "a proposal is not a candidate");

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