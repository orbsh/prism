//! prism binary: boot the engine + prism's own account registry,
//! register the echoes, serve WS on PRISM_ADDR (default 127.0.0.1:8765).

use aura_engine::Engine;
use prism::{identity::Registry, nodes::Posture, Deployment, Gateway};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // THE VENUE'S MEMORY IS DEPLOYMENT STATE, so it outlives the process the way the registry
    // does: settings and model rows are what a deployment IS, and a `memory` plane lost them on
    // every restart — the first turn after a boot then said "this deployment has no settings row"
    // and the operator re-typed the same four fields. `PRISM_ENGINE=memory` is still there for the
    // volatility someone wants (tests, a throwaway venue); absent, a build with the `fjall`
    // feature keeps its own state under `PRISM_DATA`.
    let dir = std::env::var("PRISM_DATA").unwrap_or_else(|_| "prism-data".into());
    let want = std::env::var("PRISM_ENGINE").unwrap_or_else(|_| "fjall".into());
    // A mount's runner reaches its node BY REFERENCE, and the venue is the party that holds the
    // bytes: it serves them at `/code/<hex>` on this listener, and this is where the dispatch is
    // told. Only read when mounts are declared, so a venue with no node code needs no address.
    let addr = std::env::var("PRISM_ADDR").unwrap_or_else(|_| "127.0.0.1:8765".into());
    let code_base = prism::mounts::code_base(&addr)?;
    // THE VENUE TELLS ITS GUESTS WHERE IT IS. A booth runs inside us and needs the venue's own
    // answer to "what can I call" (gravity ADR-0001 §7): publishing the address here means a page's
    // turn reads the same table the CLI does, instead of a copy riding each call — and the guest
    // inherits this process's environment, which is the only channel we have to it. An address a
    // guest cannot use (a proxy, another name) is overridden with `PRISM_VENUE_URL`.
    if std::env::var_os("PRISM_VENUE_URL").is_none() {
        std::env::set_var("PRISM_VENUE_URL", prism::mounts::venue_origin(&addr)?);
    }
    let engine_config = match (want.as_str(), cfg!(feature = "fjall")) {
        ("memory", _) => aura_config::EngineConfig {
            code_base_url: Some(code_base.clone()),
            ..Default::default()
        },
        ("fjall", true) => aura_config::EngineConfig {
            engine: aura_config::Engine::Fjall,
            data_dir: Some(std::path::Path::new(&dir).join("engine")),
            code_base_url: Some(code_base.clone()),
            ..Default::default()
        },
        ("fjall", false) => anyhow::bail!("PRISM_ENGINE=fjall needs this build's `fjall` feature"),
        (other, _) => anyhow::bail!("PRISM_ENGINE `{other}` is neither `memory` nor `fjall`"),
    };
    let engine = Engine::start(&engine_config).await?;
    // The REGISTRY first: it is durable by nature — accounts, devices and bindings outlive the
    // process (the whole §2 restore story depends on it) — and the auth booth reads the same
    // instance (accounts ns 50, verifiers ns 55).
    let registry = Registry::open(std::path::Path::new(&dir))?;
    // The shipped auth booth: the verbs stay here, the proof lives behind one call.
    Gateway::with_auth(&engine, &registry).await?;
    Gateway::with_echoes(&engine).await?;
    // The node mounts (ADR-0001) — installed after construction, because the table lives on the
    // gateway (ADR-0021), and derived from PRISM_LANGUAGES × the approved rows (ADR-0022).
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
    gw.install_mounts().await?;

    let addr = std::env::var("PRISM_ADDR").unwrap_or_else(|_| "127.0.0.1:8765".into());
    let listener = TcpListener::bind(&addr).await?;
    println!(
        "prism: ws://{addr}/ws — codec negotiated per connection \
         (Sec-WebSocket-Protocol `accrete-json`|`accrete-cbor` → ?codec= → ?protocol= → json; \
         PRISM_CODECS pins the deployment, unknown names refused 4xx); \
         verbs: echo (steel|python|nushell), echo_sender, signup, login, broadcast, ping; \
         static: GET /code/<sha256> (ADR-0027 export); renderer leg: /channel (Message/Outflow)"
    );
    gw.serve(listener).await
}
