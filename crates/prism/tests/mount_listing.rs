//! What the venue can call, published (ADR-0021), and where a mount comes from (ADR-0022).
//!
//! A mount is the PRODUCT of two facts with different owners: the deployment's language
//! vocabulary (`PRISM_LANGUAGES` — which languages this installation runs, with which runner
//! artifact) and the node registry's approved aliases (which machines are nodes). Nothing a
//! skill, an operation or a dialling robot says can add an entry to either: the vocabulary is
//! declared, an alias is approved. A machine that is merely pending gets no type — approval is
//! what makes a node callable.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use prism::nodes::Claim;
use prism::{identity::Registry, Gateway};
use serde_json::Value;

/// The deployment's vocabulary: what an installation declares once. The default for a node is the
/// WHOLE vocabulary — a machine that lacks python's runner fails on the call, and that is the
/// environment's problem to fix, not a reason to make every operator tick boxes.
const VOCABULARY: &str = r#"[
  {"language": "nu", "carrier": "induction", "runner": "/runners/runner_nu.nu", "encoding": "cbor"},
  {"language": "python", "carrier": "exec", "runner": "/runners/runner_py.py"}
]"#;

/// The same entry with a field missing: a declaration is a deployment FACT (gravity ADR-0001), so
/// absence is a boot error and never a default.
const VOCABULARY_BROKEN: &str = r#"[
  {"language": "nu", "carrier": "induction"}
]"#;

async fn boot() -> (std::net::SocketAddr, std::sync::Arc<Gateway>) {
    std::env::set_var("PRISM_LANGUAGES", VOCABULARY);
    let registry = Registry::open(&tempfile::tempdir().unwrap().keep()).unwrap();
    let engine = aura_engine::Engine::start(&aura_config::EngineConfig::default())
        .await
        .expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, prism::nodes::Posture::Open);
    gw.install_mounts().await.expect("the vocabulary installs");
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

/// Approve an alias the way the operator does, without the HTTP round trip: the registry row is
/// the only fact the derivation reads.
fn approve(gw: &Gateway, alias: &str) {
    gw.nodes()
        .create(&Claim {
            alias: alias.into(),
            credential: prism::nodes::token_credential("a-token-for-this-test"),
            machine: String::new(),
            labels: vec!["u1:MyLaptop".into()],
            owner: "u1".into(),
        })
        .expect("mint");
    gw.nodes()
        .approve(alias, None)
        .expect("the sole pending record approves");
}

#[tokio::test]
async fn a_mount_is_the_product_of_the_vocabulary_and_an_approved_alias() {
    let (addr, gw) = boot().await;

    // The vocabulary is deployed and no node is approved: nothing is callable, and the page says
    // so instead of inventing an entry.
    let v = mounts(addr).await;
    assert_eq!(v["mounts"].as_array().unwrap().len(), 0, "{v}");
    assert_eq!(v["refused"].as_array().unwrap().len(), 0, "{v}");

    // Approve a node: callable as `{alias}.{language}`, once per vocabulary entry.
    approve(&gw, "u1-mbp");
    gw.install_mounts().await.expect("replay at the next boot");
    let v = mounts(addr).await;
    let rows = v["mounts"].as_array().expect("mounts");
    assert_eq!(rows.len(), 2, "one mount per language: {v}");
    let nu = rows
        .iter()
        .find(|r| r["type"] == "u1-mbp.nu")
        .expect("the nu mount");
    assert_eq!(nu["alias"], "u1-mbp");
    assert_eq!(nu["language"], "nu");
    assert_eq!(nu["carrier"], "induction");
    assert_eq!(nu["encoding"], "cbor");
    assert_eq!(nu["requires"], "container", "absent means the container");
    assert_eq!(nu["registered"], true);
    assert_eq!(
        nu["node_connected"], false,
        "approved is not connected: the listing has to say both"
    );
    let py = rows
        .iter()
        .find(|r| r["type"] == "u1-mbp.python")
        .expect("the python mount");
    assert_eq!(py["encoding"], "json", "the default codec");

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
            carriers: vec!["induction".into(), "exec".into()],
            class: effector_protocol::NodeClass::Host,
            machine: "box-mbp".into(),
            labels: vec!["u1:MyLaptop".into()],
        });
    let v = mounts(addr).await;
    let nu = v["mounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["type"] == "u1-mbp.nu")
        .unwrap();
    assert_eq!(
        nu["node_connected"], true,
        "presence is not registration: the same page has to say both"
    );

    // In `open` posture there is no registry, so the dial IS the alias list — and the types are
    // minted from the vocabulary, never from anything the node said about itself.
    let added = gw.ensure_alias_mounts("stranger-box").await;
    assert_eq!(added, vec!["stranger-box.nu", "stranger-box.python"]);
    assert_eq!(
        gw.ensure_alias_mounts("stranger-box").await.len(),
        0,
        "a second dial adds nothing: derivation is idempotent"
    );

    // THE LAST MILE OF DELIVERY (ADR-0027): a mount's runner reaches its node by reference, so
    // the venue must be able to serve the bytes it declares — on its own listener, at the address
    // the dispatch is told. Without this the mount exists and every call fails three layers down
    // with "no code_base_url configured".
    let spec = "/runners/runner_nu.nu";
    let hex = aura_realm::meta::code_hex(&aura_realm::meta::code_hash(spec));
    let served = get(addr, &format!("/code/{hex}")).await;
    assert_eq!(served, spec, "the venue serves the runner it declared");

    // A vocabulary with a field missing is a boot error, and the venue keeps what it already had —
    // half a table would be worse than none.
    std::env::set_var("PRISM_LANGUAGES", VOCABULARY_BROKEN);
    let err = prism::mounts::vocabulary_from_env().expect_err("no runner, no mount");
    assert!(
        format!("{err:#}").contains("missing string `runner`"),
        "the error names the field: {err:#}"
    );
    assert_eq!(gw.mounts().len(), 4, "the installed table is untouched");
}
/// The base the dispatch hands the node: derived from the bind address, because the venue serves
/// its own specs; a bind-all address is refused rather than guessed at, and an explicit base wins.
#[test]
fn the_code_base_is_an_address_a_node_can_fetch() {
    std::env::remove_var("PRISM_CODE_BASE");
    assert_eq!(
        prism::mounts::code_base("127.0.0.1:8765").unwrap(),
        "http://127.0.0.1:8765/code"
    );
    assert_eq!(
        prism::mounts::code_base("10.0.0.5:8765").unwrap(),
        "http://10.0.0.5:8765/code"
    );
    let err =
        prism::mounts::code_base("0.0.0.0:8765").expect_err("a listener is not a destination");
    assert!(
        format!("{err:#}").contains("PRISM_VENUE_URL"),
        "the refusal names the fix: {err:#}"
    );
    // The ORIGIN is what a guest reads the table from and what a node is told about; the CODE base
    // is that origin plus one path, so a deployment that names only the origin gets both.
    assert_eq!(
        prism::mounts::venue_origin("127.0.0.1:8765").unwrap(),
        "http://127.0.0.1:8765"
    );
    assert_eq!(
        prism::mounts::code_base("127.0.0.1:8765").unwrap(),
        "http://127.0.0.1:8765/code"
    );
    std::env::set_var("PRISM_VENUE_URL", "https://venue.example/");
    assert_eq!(
        prism::mounts::venue_origin("0.0.0.0:8765").unwrap(),
        "https://venue.example",
        "the deployment's word beats derivation, and a trailing slash is not a second path"
    );
    assert_eq!(
        prism::mounts::code_base("0.0.0.0:8765").unwrap(),
        "https://venue.example/code",
        "a named origin carries the code base with it"
    );
    std::env::set_var("PRISM_CODE_BASE", "https://cdn.example/specs/");
    assert_eq!(
        prism::mounts::code_base("0.0.0.0:8765").unwrap(),
        "https://cdn.example/specs",
        "a CDN in front of the specs is its own address"
    );
    std::env::remove_var("PRISM_CODE_BASE");
    std::env::remove_var("PRISM_VENUE_URL");
}
