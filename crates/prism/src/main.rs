//! prism binary: boot the engine + prism's own account registry,
//! register the echoes, serve WS on PRISM_ADDR (default 127.0.0.1:8765).

use aura_engine::Engine;
use prism::{
    identity::Registry,
    nodes::Posture,
    Gateway,
};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Demo config surface: the memory plane for actors (persistence is
    // aura's own fjall switch); the REGISTRY is durable by nature —
    // accounts and device bindings outlive the process (the whole §2
    // restore story depends on it).
    let engine = Engine::start(&Default::default()).await?;
    Gateway::with_echoes(&engine).await?;
    let dir = std::env::var("PRISM_DATA").unwrap_or_else(|_| "prism-data".into());
    let registry = Registry::open(std::path::Path::new(&dir))?;
    // The trust posture is DECLARED, never defaulted (ADR-0015 §7):
    // absent or invalid PRISM_IDENTITY is a boot error, not a guess.
    let posture_raw = std::env::var("PRISM_IDENTITY")
        .map_err(|_| anyhow::anyhow!("PRISM_IDENTITY is required: `required` | `open`"))?;
    let posture = Posture::parse(&posture_raw)?;
    let gw = Gateway::new(engine, registry, posture);

    let addr = std::env::var("PRISM_ADDR").unwrap_or_else(|_| "127.0.0.1:8765".into());
    let listener = TcpListener::bind(&addr).await?;
    println!(
        "prism: ws://{addr}/ws — CBOR default, ?protocol=json for debug; \
         verbs: echo (steel|python|nushell|wasm), echo_sender, signup, login, broadcast, ping; \
         static: GET /code/<sha256> (ADR-0027 export)"
    );
    gw.serve(listener).await
}
