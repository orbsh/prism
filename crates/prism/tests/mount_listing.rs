//! What the venue can call, published (ADR-0021).
//!
//! The mount table used to be a manifest that every client kept its own copy of: gravity
//! carried one in its turn args, prism replayed one at boot, and two clients of one venue
//! disagreeing was a matter of time. The venue holds the table, so it answers for it —
//! declared, registered, and whether the node behind it is connected, because those are three
//! different facts and a caller that knows only the first will choose a mount that cannot run.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use prism::{identity::Registry, Gateway};
use serde_json::Value;

/// Two declared mounts on one laptop, one per language — the shape gravity ADR-0001 asks for.
const MANIFEST: &str = r#"[
  {"type": "laptop.nu", "alias": "u1-mbp", "carrier": "induction", "language": "nu",
   "runner": "/runners/runner_nu.nu"},
  {"type": "laptop.python", "alias": "u1-mbp", "carrier": "exec", "language": "python",
   "runner": "/runners/runner_py_guest.py"}
]"#;

/// The same manifest with a field missing. A declaration is a deployment FACT (gravity ADR-0001):
/// absent is never a default, so this is a boot error and not a skipped entry.
const MANIFEST_BROKEN: &str = r#"[
  {"type": "laptop.nu", "alias": "u1-mbp", "carrier": "induction", "language": "nu"}
]"#;

async fn boot() -> (std::net::SocketAddr, std::sync::Arc<Gateway>) {
    std::env::set_var("PRISM_MOUNTS", MANIFEST);
    let registry = Registry::open(&tempfile::tempdir().unwrap().keep()).unwrap();
    let engine = aura_engine::Engine::start(&aura_config::EngineConfig::default())
        .await
        .expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, prism::nodes::Posture::Open);
    gw.install_mounts().await.expect("the manifest installs");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = gw.clone();
    tokio::spawn(async move { served.serve(listener).await.unwrap() });
    (addr, gw)
}

/// A plain HTTP GET: `/admin` answers and closes, so a hand-rolled request is the whole client.
async fn get(addr: std::net::SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).to_string();
    text.split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default()
}

async fn mounts(addr: std::net::SocketAddr) -> Value {
    let body = get(addr, "/admin/mounts").await;
    serde_json::from_str(&body).expect("the listing is JSON")
}

#[tokio::test]
async fn the_venue_publishes_what_it_can_call_and_what_it_could_not() {
    let (addr, gw) = boot().await;

    let v = mounts(addr).await;
    let rows = v["mounts"].as_array().expect("mounts");
    assert_eq!(rows.len(), 2, "every declaration is listed, live or not: {v}");
    let live = rows
        .iter()
        .find(|r| r["type"] == "laptop.nu")
        .expect("the valid declaration");
    assert_eq!(live["alias"], "u1-mbp");
    assert_eq!(live["language"], "nu");
    assert_eq!(live["carrier"], "induction");
    assert_eq!(live["requires"], "container", "absent means the container");
    assert_eq!(live["registered"], true);
    assert_eq!(
        live["node_connected"], false,
        "declared and registered, but the node has not dialled yet"
    );

    assert_eq!(
        v["refused"].as_array().expect("refused").len(),
        0,
        "both registered: {v}"
    );

    // The node dials: presence is the pool's fact, and the listing follows it.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    drop(rx);
    gw.engine
        .realm_set
        .pool()
        .insert(aura_realm::pool::NodeRecord {
            alias: "u1-mbp".into(),
            conn: aura_realm::ActuatorConn {
                sender: tx,
                peer: std::net::SocketAddr::from(([127, 0, 0, 1], 1)),
            },
            carriers: vec!["induction".into()],
            class: effector_protocol::NodeClass::Host,
            machine: "box-mbp".into(),
            labels: vec!["u1:MyLaptop".into()],
        });
    let v = mounts(addr).await;
    let live = v["mounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["type"] == "laptop.nu")
        .unwrap();
    assert_eq!(
        live["node_connected"], true,
        "presence is not registration: the same page has to say both"
    );

    // A manifest with a field missing is a boot error, and a failed install leaves the venue
    // holding what it already had — half a table would be worse than none.
    std::env::set_var("PRISM_MOUNTS", MANIFEST_BROKEN);
    let err = prism::mounts::MountTable::install(&gw.engine)
        .await
        .expect_err("a declaration without a runner cannot be guessed");
    assert!(
        format!("{err:#}").contains("missing string `runner`"),
        "the error names the field: {err:#}"
    );
    assert_eq!(
        gw.mounts().len(),
        2,
        "the previously installed table is untouched"
    );
}