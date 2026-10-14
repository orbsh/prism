//! The runner mount e2e (gravity ADR-0001): a mount's runner is FIXED and an
//! operation's code travels as a CALL ARGUMENT.
//!
//! A real effector node dials prism's `/effector/<alias>`; prism registers the
//! mount from a manifest declaration through the production face
//! (`prism::mounts::register_all`); then two DIFFERENT operations run through
//! the same type — no re-registration between them, no per-skill type, no code
//! reference for the operation itself (the mount's source is the runner, and
//! only the runner). The delivered script prints one JSON document; a failed
//! script returns an error VALUE rather than a masked carrier failure.

use aura_booth::{Body, InstanceId, InstanceKey};
use prism::{identity::Registry, mounts::MountDecl, Gateway};
use std::time::Duration;

/// ADR-0027: the node fetches its code by content reference. Here the mount's
/// source IS a short argv spec (the process carrier's contract), so the test
/// serves those bytes — the fetch matrix itself is locked effector-side.
fn serve_source(src: &str) -> u16 {
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

/// The deployment artifact the node installs (ADR-0001 §4: a runner is
/// infrastructure — fixed code, shipped with the node, never a graph citizen).
fn nu_runner_path() -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../effector/deploy/runners/runner_nu.nu"
    );
    assert!(
        std::path::Path::new(path).exists(),
        "the nu runner artifact must exist at {path}"
    );
    path.to_string()
}

#[tokio::test]
async fn a_mount_runs_delivered_code_with_zero_per_operation_registration() {
    let runner = nu_runner_path();
    // The mount's source: the process carrier's argv spec — the runner, and
    // only the runner. It is identical for every operation this mount serves.
    let spec = format!("nu {runner}");
    let http_port = serve_source(&spec);

    let registry = Registry::open(&tempfile::tempdir().unwrap().keep()).unwrap();
    let engine = aura_engine::Engine::start(&aura_config::EngineConfig {
        code_base_url: Some(format!("http://127.0.0.1:{http_port}")),
        ..Default::default()
    })
    .await
    .expect("engine boot");
    let gw = Gateway::new(engine, registry, prism::nodes::Posture::Open);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let engine = gw.engine.clone();
    tokio::spawn(async move { gw.serve(listener).await.unwrap() });

    // The declaration → the production registration face.
    let decl = MountDecl {
        type_name: "runner_nu".into(),
        node_alias: "runner-node".into(),
        language: "exec".into(),
        runner: spec.clone(),
        encoding: aura_booth::Encoding::Json,
    };
    let live = prism::mounts::register_all(&engine, std::slice::from_ref(&decl))
        .await
        .unwrap();
    assert_eq!(live, vec!["runner_nu".to_string()], "the mount registered");

    // The node dials in (a real effector, node side of the same stack).
    std::env::set_var("PRISM_RUNNER_E2E_CREDENTIAL", "tok");
    let config = effector_config::EffectorConfig {
        control_plane_url: format!("ws://127.0.0.1:{}/effector/runner-node", addr.port()),
        // The policy belongs to the deployment; this test locks the call path.
        sandbox: false,
        credential_env: "PRISM_RUNNER_E2E_CREDENTIAL".into(),
        capabilities: effector_config::CapabilitySurface {
            node_alias: "runner-node".into(),
            carriers: vec!["exec".into()],
            ..Default::default()
        },
    };
    let _node = tokio::spawn(effector_runtime::remote::run(config));
    for _ in 0..50 {
        if engine
            .realm
            .try_lock()
            .unwrap()
            .effectors
            .contains_key("runner-node")
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
            .contains_key("runner-node"),
        "the node registered through /effector/<alias>"
    );

    let target = InstanceId {
        booth_type: "runner_nu".into(),
        key: InstanceKey::Named("session-1".into()),
    };
    let invoke = |code: &str| {
        let target = target.clone();
        let engine = engine.clone();
        let code = code.to_string();
        async move {
            engine
                .invoke(
                    target,
                    "execute",
                    serde_json::json!({ "code": code, "args": { "n": 4 } }),
                )
                .await
        }
    };

    // Operation 1: the delivered script runs on the node.
    let out = invoke("{sum: (($env.TOOLRUNNER_ARGS | from json | get n) + 3)} | to json")
        .await
        .unwrap();
    assert_eq!(
        out,
        serde_json::json!({ "sum": 7 }),
        "code travelled as an argument"
    );

    // Operation 2: a DIFFERENT script through the SAME type — the mount was
    // never re-registered, and its source is still the runner alone.
    let out = invoke("{up: (\"hi\" | str upcase)} | to json")
        .await
        .unwrap();
    assert_eq!(out, serde_json::json!({ "up": "HI" }));

    // The mount's source never moved: one registration, many operations.
    let source_now = match &engine
        .realm
        .try_lock()
        .unwrap()
        .booth_type("runner_nu")
        .expect("mount type present")
        .body
    {
        Body::RemoteProbe { source, .. } => source.clone(),
        _ => panic!("a mount's body is a remote arm"),
    };
    assert_eq!(source_now, spec, "the runner is fixed across operations");

    // A mount whose language this node was not built with: the carrier names
    // it, per call, as an error — the manifest↔carriers note at dial-in exists
    // to catch this drift early, and it is not a refusal mechanism.
    let unbuilt = MountDecl {
        type_name: "runner_unbuilt".into(),
        node_alias: "runner-node".into(),
        language: "wasmtime".into(),
        runner: spec.clone(),
        encoding: aura_booth::Encoding::Json,
    };
    prism::mounts::register_all(&engine, std::slice::from_ref(&unbuilt))
        .await
        .unwrap();
    let err = engine
        .invoke(
            InstanceId {
                booth_type: "runner_unbuilt".into(),
                key: InstanceKey::Named("session-1".into()),
            },
            "execute",
            serde_json::json!({ "code": "{ok: true} | to json" }),
        )
        .await
        .expect_err("a language the node does not carry is an error, not a silent miss");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("wasmtime"),
        "the carrier names the un-carried language, got: {msg}"
    );

    // A failing script is a VALUE going into the next inference round, never
    // a masked carrier failure (ADR-0012 at the runner boundary).
    let out = invoke("error make {msg: \"boom\"}").await.unwrap();
    let err = out
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or_default();
    assert!(
        err.starts_with("runner_nu: delivered code failed"),
        "the runner reports its own failure text, got: {out}"
    );
}

/// The language↔node fact (ADR-0001): the manifest declares what a node's
/// mounts expect, the dial-in frame announces what the node was BUILT with,
/// and a mismatch is a stated line rather than a silent one.
#[test]
fn the_manifest_and_the_announced_carriers_are_compared() {
    let mount = |ty: &str, lang: &str| MountDecl {
        type_name: ty.into(),
        node_alias: "n1".into(),
        language: lang.into(),
        runner: "nu /opt/runner/runner_nu.nu".into(),
        encoding: aura_booth::Encoding::Json,
    };
    let mounts = vec![
        mount("runner_nu", "exec"),
        mount("runner_nu_ind", "induction"),
    ];

    let line = prism::mounts::drift_line(&mounts, "n1", &["exec".to_string()])
        .expect("a node missing a declared carrier is reported");
    assert!(
        line.contains("induction") && line.contains("not carried"),
        "the line names what is missing: {line}"
    );

    assert!(
        prism::mounts::drift_line(
            &mounts,
            "n1",
            &["exec".to_string(), "induction".to_string()]
        )
        .is_none(),
        "agreement is silent"
    );
    assert!(
        prism::mounts::drift_line(&mounts, "other-node", &["exec".to_string()]).is_none(),
        "an alias nothing is declared for is silent"
    );
}
