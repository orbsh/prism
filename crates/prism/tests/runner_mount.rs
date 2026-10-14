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

/// ADR-0027: the node fetches its code by content reference — `{hash}` under
/// the deployment's code base URL. Each mount's source is a short argv spec
/// (the process carrier's contract), so the test serves exactly those bytes,
/// addressed by the hash the control plane computes.
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

/// The deployment artifacts the node installs (ADR-0001 §4: a runner is
/// infrastructure — fixed code, shipped with the node, never a graph citizen).
/// Two languages, two runners: the mount name carries the operation's
/// language, the manifest's `language` carries the carrier both ride.
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

#[tokio::test]
async fn a_mount_runs_delivered_code_with_zero_per_operation_registration() {
    // Each mount's source: the process carrier's argv spec — the runner, and
    // only the runner. It is identical for every operation that mount serves.
    let nu_spec = format!("nu {}", runner_path("runner_nu.nu"));
    let py_spec = format!("python3 {}", runner_path("runner_py.py"));
    let resident_spec = format!("python3 {}", runner_path("runner_py_guest.py"));
    let http_port = serve_sources(&[nu_spec.clone(), py_spec.clone(), resident_spec.clone()]);

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

    // The declarations → the production registration face (two languages, two
    // mounts, one node).
    let decl = |ty: &str, language: &str, runner: &str| MountDecl {
        type_name: ty.into(),
        node_alias: "runner-node".into(),
        language: language.into(),
        runner: runner.to_string(),
        encoding: aura_booth::Encoding::Json,
    };
    // The one-shot runners ride the `exec` carrier; the resident guest rides
    // `induction` — its wire is the frame protocol, not one request/response.
    let decls = vec![
        decl("runner_nu", "exec", &nu_spec),
        decl("runner_py", "exec", &py_spec),
    ];
    let live = prism::mounts::register_all(&engine, &decls).await.unwrap();
    assert_eq!(
        live,
        vec!["runner_nu".to_string(), "runner_py".to_string()],
        "both mounts registered"
    );

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
    assert_eq!(source_now, nu_spec, "the runner is fixed across operations");

    // The other language, its own mount: same call shape, a different runner.
    let py_out = engine
        .invoke(
            InstanceId {
                booth_type: "runner_py".into(),
                key: InstanceKey::Named("session-1".into()),
            },
            "execute",
            serde_json::json!({
                "code": "import json, os\nn = json.loads(os.environ[\"TOOLRUNNER_ARGS\"])[\"n\"]\nprint(json.dumps({\"doubled\": n * 2}))",
                "args": { "n": 21 }
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        py_out,
        serde_json::json!({ "doubled": 42 }),
        "the python mount ran the delivered script"
    );

    // The resident form (ADR-0001 §8): the delivered script runs IN the guest
    // process, so it (a) keeps state across calls — what the one-shot form
    // cannot — and (b) can call the host's ctx bridge, here into the nu mount
    // above (a sibling reached through the realm, not by a second connection).
    let resident = decl("runner_py_resident", "induction", &resident_spec);
    prism::mounts::register_all(&engine, std::slice::from_ref(&resident))
        .await
        .unwrap();
    let resident_target = InstanceId {
        booth_type: "runner_py_resident".into(),
        key: InstanceKey::Named("session-1".into()),
    };
    let counter =
        "import json\nSTATE['n'] = STATE.get('n', 0) + 1\nprint(json.dumps({'n': STATE['n']}))";
    let first = engine
        .invoke(
            resident_target.clone(),
            "execute",
            serde_json::json!({ "code": counter }),
        )
        .await
        .unwrap();
    assert_eq!(
        first,
        serde_json::json!({ "n": 1 }),
        "the resident runner ran it"
    );
    let second = engine
        .invoke(
            resident_target.clone(),
            "execute",
            serde_json::json!({ "code": counter }),
        )
        .await
        .unwrap();
    assert_eq!(
        second,
        serde_json::json!({ "n": 2 }),
        "guest-scope state survives between calls (residency, not a fresh process)"
    );

    // The ctx bridge out of the resident guest (ADR-0001 §8): the delivered
    // script calls the sibling nu mount through the host realm and gets its
    // VALUE back. It runs on a session that has ALREADY served two calls —
    // the bridge is per-call state (it carries the enclosing remote call's
    // id), so a session that kept only its cold-start bridge would answer
    // "unknown call_id" here instead of reaching the sibling.
    let bridged = engine
        .invoke(
            resident_target.clone(),
            "execute",
            serde_json::json!({
                "code": "import json\nr = ctx_invoke('runner_nu', 'session-1', 'execute', \
                         {'code': '{sum: (($env.TOOLRUNNER_ARGS | from json | get n) + 3)} | to json', \
                         'args': {'n': 4}})\nprint(json.dumps({'bridged': r}))"
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        bridged,
        serde_json::json!({ "bridged": { "sum": 7 } }),
        "the resident guest's ctx_invoke reached the sibling mount through the host realm"
    );

    // A mount whose language this node was not built with: the carrier names
    // it, per call, as an error — the manifest↔carriers note at dial-in exists
    // to catch this drift early, and it is not a refusal mechanism.
    let unbuilt = MountDecl {
        type_name: "runner_unbuilt".into(),
        node_alias: "runner-node".into(),
        language: "wasmtime".into(),
        runner: nu_spec.clone(),
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

    // ADR-0001 §3: entry `execute` is a CONVENTION, never a lookup — nothing
    // validates the name (effector keeps no table), so a caller that invents
    // one still lands on the delivered code, and the name it sent is DATA the
    // script can read rather than a key that decides dispatch. The one-shot
    // runner hands it over as TOOLRUNNER_ENTRY; the resident runner serves its
    // one job under any name at all.
    let out = engine
        .invoke(
            InstanceId {
                booth_type: "runner_nu".into(),
                key: InstanceKey::Named("session-1".into()),
            },
            "the_caller_made_this_name_up",
            serde_json::json!({
                "code": "{entry: $env.TOOLRUNNER_ENTRY, ran: true} | to json",
                "args": {}
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        serde_json::json!({ "entry": "the_caller_made_this_name_up", "ran": true }),
        "an unrecognised entry name still runs the delivered code"
    );

    let out = engine
        .invoke(
            resident_target.clone(),
            "the_caller_made_this_name_up",
            serde_json::json!({ "code": "import json\nprint(json.dumps({'ran': True}))" }),
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        serde_json::json!({ "ran": true }),
        "the resident runner serves its one job under any entry name too"
    );

    // ctx.emit out of the resident guest (ADR-0043 §5 — fire-and-forget by
    // contract): the arm IS answered over the wire (reply pairing is protocol),
    // but the delivered script reads nothing and keeps running. A mount
    // declares no routes, so an emit from one reaches no consumer — and the
    // fact is not dropped silently: it lands in the dead ring with its reason
    // (ADR-0038 §4), which is what makes the leg assertable here.
    let out = engine
        .invoke(
            resident_target.clone(),
            "execute",
            serde_json::json!({
                "code": "import json\nctx_emit('mount.something_happened', {'n': 1})\n\
                         print(json.dumps({'emitted': True}))"
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        out,
        serde_json::json!({ "emitted": true }),
        "an emit does not park the delivered script (no reply is read)"
    );
    let dead = engine.realm.lock().await.dead_events.detailed();
    assert!(
        dead.iter()
            .any(|(event, data, reason)| event == "mount.something_happened"
                && data == &serde_json::json!({ "n": 1 })
                && *reason == aura_realm::event::DeadReason::NoRoute),
        "the emit is observable with its reason (never silently dropped), got: {dead:?}"
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
