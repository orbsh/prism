//! prism binary: boot the engine, register the four-language echoes,
//! serve WS on PRISM_ADDR (default 127.0.0.1:8765).

use aura_engine::Engine;
use prism::Gateway;

use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Demo config surface: the memory plane (persistent fjall is the
    // aura-side concern; the echo plane holds no state across boots).
    let engine = Engine::start(&Default::default()).await?;
    Gateway::with_echoes(&engine).await?;
    let gw = Gateway::new(engine);

    let addr = std::env::var("PRISM_ADDR").unwrap_or_else(|_| "127.0.0.1:8765".into());
    let listener = TcpListener::bind(&addr).await?;
    println!(
        "prism: ws://{addr}/ws — CBOR default, ?protocol=json for debug; \
         verbs: echo (steel|python|nushell|wasm), broadcast, ping"
    );
    gw.serve(listener).await
}
