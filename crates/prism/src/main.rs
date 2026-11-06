//! prism binary: boot the engine + prism's own account registry,
//! register the echoes, serve WS on PRISM_ADDR (default 127.0.0.1:8765).

use aura_engine::Engine;
use prism::{
    identity::Registry,
    nodes::Posture,
    Deployment, Gateway,
};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Demo config surface: the memory plane for booths (persistence is
    // aura's own fjall switch); the REGISTRY is durable by nature —
    // accounts and device bindings outlive the process (the whole §2
    // restore story depends on it).
    let engine = Engine::start(&Default::default()).await?;
    // The REGISTRY first: it is durable by nature — accounts, devices and bindings outlive the
    // process (the whole §2 restore story depends on it) — and the auth booth reads the same
    // instance (accounts ns 50, verifiers ns 55).
    let dir = std::env::var("PRISM_DATA").unwrap_or_else(|_| "prism-data".into());
    let registry = Registry::open(std::path::Path::new(&dir))?;
    // The shipped auth booth: the verbs stay here, the proof lives behind one call.
    Gateway::with_auth(&engine, &registry).await?;
    Gateway::with_echoes(&engine).await?;
    // The declared node mounts (ADR-0001): absent PRISM_MOUNTS = no nodes.
    Gateway::with_mounts(&engine).await?;
    // The trust posture is DECLARED, never defaulted (ADR-0015 §7):
    // absent or invalid PRISM_IDENTITY is a boot error, not a guess.
    let posture_raw = std::env::var("PRISM_IDENTITY")
        .map_err(|_| anyhow::anyhow!("PRISM_IDENTITY is required: `required` | `open`"))?;
    let posture = Posture::parse(&posture_raw)?;
    // The front-end face (ADR-0017 §10): the renderer's bundle. A deployment fact with a legal
    // absence — a venue with no bundle still serves identity, turns and the peer leg, and the
    // boot log says which faces are live. The layout itself is the application's answer to
    // `ui:connect`, not something a deployment declares.
    let deployment = Deployment::from_env()?;
    for line in deployment.describe() {
        println!("{line}");
    }
    if deployment.assets.is_none() {
        println!("prism: no renderer bundle (PRISM_ASSETS unset) — / serves nothing");
    }
    let gw = Gateway::with_deployment(engine, registry, posture, deployment);

    let addr = std::env::var("PRISM_ADDR").unwrap_or_else(|_| "127.0.0.1:8765".into());
    let listener = TcpListener::bind(&addr).await?;
    println!(
        "prism: ws://{addr}/ws — CBOR default, ?protocol=json for debug; \
         verbs: echo (steel|python|nushell), echo_sender, signup, login, broadcast, ping; \
         static: GET /code/<sha256> (ADR-0027 export); renderer leg: /channel (Message/Outflow)"
    );
    gw.serve(listener).await
}
