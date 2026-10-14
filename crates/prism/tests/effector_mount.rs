//! Effector mount e2e (Phase 1.8 step 2, ADR-0017 §6): a REAL effector
//! dials prism at `/effector/<alias>` — the same URL the handshake will
//! gate later — registers into the realm's effector table, and a remote
//! booth call round-trips through the wire (the aura remote_effector
//! shape, mounted on prism's shared accept loop instead of a dedicated
//! port). Alias-mismatch and disconnect-presence are locked alongside:
//! the path and the frame state the alias twice (one fact), and the
//! registration dies with the connection (replacement discipline,
//! ADR-0015 §7).

use aura_booth::{Body, BoothType, InstanceId, InstanceKey};
use prism::{identity::Registry, Gateway};
use std::collections::BTreeMap;
use std::time::Duration;

/// ADR-0027: remote code travels by content reference; the test serves
/// the handler bytes (the fetch matrix is locked effector-side in
/// remote.rs).
fn serve_code_source(src: &str) -> u16 {
    use std::io::{Read as _, Write as _};
    let bytes = src.as_bytes().to_vec();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let body = bytes.clone();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    port
}

async fn boot() -> (std::net::SocketAddr, aura_engine::Engine) {
    let registry = Registry::open(&tempfile::tempdir().unwrap().keep()).unwrap();
    let engine = aura_engine::Engine::start(&aura_config::EngineConfig::default())
        .await
        .expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, prism::nodes::Posture::Open);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let engine = gw.engine.clone() /* Engine is Clone? see below */;
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });
    (addr, engine)
}

/// The real effector's dial config, pointed at prism's mount path.
fn effector_config(port: u16, alias: &str) -> effector_config::EffectorConfig {
    effector_config::EffectorConfig {
        control_plane_url: format!("ws://127.0.0.1:{port}/effector/{alias}"),
        // The call path, not the sandbox (steel runs in-process).
        sandbox: true,
        credential_env: "PRISM_MOUNT_E2E_CREDENTIAL".into(),
        capabilities: effector_config::CapabilitySurface {
            node_alias: alias.into(),
            carriers: vec!["steel".into()],
            ..Default::default()
        },
    }
}

const SRC: &str = r#"
(define (double args)
  (let* ((echoed (ctx.invoke (hash "type" "echo" "key" "e1" "handler" "execute" "args" (hash "x" 1)))))
    (hash "doubled" (* 2 (hash-ref args "n"))
          "echo" (hash-ref echoed "x"))))
"#;

#[tokio::test]
async fn effector_mount_roundtrip_and_presence() {
    let (addr, engine) = boot().await;

    // This test locks registration + the presence flip; the remote-booth
    // round trip (which needs `code_base_url`) is its own test below, and it
    // is the one that serves code.

    // Local target for the effector's ctx.invoke.
    engine
        .register(BoothType::script(
            "echo",
            "steel",
            r#"(define (execute args) args)"#,
        ))
        .await
        .unwrap();

    // Effector side: dial prism's mount (runs until the test ends).
    std::env::set_var("PRISM_MOUNT_E2E_CREDENTIAL", "tok");
    let effector = tokio::spawn(effector_runtime::remote::run(effector_config(
        addr.port(),
        "test-node",
    )));

    // Wait for registration through the mount.
    for _ in 0..50 {
        if engine.realm.try_lock().unwrap().effectors.contains_key("test-node") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        engine.realm.try_lock().unwrap().effectors.contains_key("test-node"),
        "the effector registered through /effector/<alias>"
    );

    // Disconnect flips presence (abort → reader EOF → guard unregisters).
    effector.abort();
    for _ in 0..50 {
        if !engine.realm.try_lock().unwrap().effectors.contains_key("test-node") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !engine.realm.try_lock().unwrap().effectors.contains_key("test-node"),
        "dropped connection unregisters the node alias"
    );
}

#[tokio::test]
async fn effector_mount_remote_call_roundtrip() {
    // This stack needs code_base_url (the remote booth fetches its body
    // by content reference), so it boots its own engine rather than
    // reusing boot()'s.
    let http_port = serve_code_source(SRC);
    let registry = Registry::open(&tempfile::tempdir().unwrap().keep()).unwrap();
    let engine = aura_engine::Engine::start(&aura_config::EngineConfig {
        code_base_url: Some(format!("http://127.0.0.1:{http_port}")),
        ..Default::default()
    })
    .await
    .expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, prism::nodes::Posture::Open);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let engine = gw.engine.clone();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });

    // The remote booth type + the local ctx.invoke target.
    engine
        .register(BoothType {
            name: "remote-counter".into(),
            body: Body::RemoteProbe {
                encoding: aura_booth::Encoding::Json,
                node_alias: "test-node".into(),
                language: "steel".into(),
                source: SRC.into(),
            },
            idle_ttl: Some(Duration::from_secs(60)),
            max_exec: None,
            hooks: BTreeMap::new(),
            receives: vec![],
        })
        .await
        .unwrap();
    engine
        .register(BoothType::script(
            "echo",
            "steel",
            r#"(define (execute args) args)"#,
        ))
        .await
        .unwrap();

    // The effector dials the mount.
    std::env::set_var("PRISM_MOUNT_E2E_CREDENTIAL", "tok");
    let _effector = tokio::spawn(effector_runtime::remote::run(effector_config(
        addr.port(),
        "test-node",
    )));
    for _ in 0..50 {
        if engine.realm.try_lock().unwrap().effectors.contains_key("test-node") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Invoke through the wire: prism's mount hands the connection to
    // the realm, the remote session executes, ctx.invoke resolves
    // against prism's own realm.
    let out = engine
        .invoke(
            InstanceId { booth_type: "remote-counter".into(), key: InstanceKey::Named("k".into()) },
            "double",
            serde_json::json!({"n": 4}),
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        serde_json::json!({"doubled": 8, "echo": 1}),
        "ctx invoke resolved over the wire through the mount"
    );
}

/// The alias-mismatch refusal: a Register whose node_alias differs from
/// the mount path is a protocol error — the connection closes, nothing
/// registers. Drives the raw WS by hand (the real effector always sends
/// its configured alias; the mismatch is a hostile/buggy client).
#[tokio::test]
async fn effector_mount_refuses_alias_mismatch() {
    let (addr, engine) = boot().await;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/effector/test-node"
    ))
    .await
    .expect("ws handshake on the mount path");

    // The WRONG alias (path said test-node).
    let frame = effector_protocol::Frame::Register {
        node_alias: "other-node".into(),
        credential: "tok".into(),
        carriers: vec!["steel".into()],
    };
    ws.send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
        .await
        .unwrap();

    // The server refuses: the socket closes with an error (any outcome
    // that is NOT a Registered frame).
    let mut refused = false;
    for _ in 0..2 {
        match ws.next().await {
            None => { refused = true; break; } // stream ended
            Some(Err(_)) => { refused = true; break; } // close frames as error
            Some(Ok(Message::Close(_))) => { refused = true; break; }
            Some(Ok(_)) => continue,
        }
    }
    assert!(refused, "alias mismatch closes the connection");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !engine.realm.try_lock().unwrap().effectors.contains_key("other-node"),
        "nothing registered under the mismatched alias"
    );
}
