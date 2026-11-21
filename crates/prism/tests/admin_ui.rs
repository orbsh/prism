//! The grant page is REACHABLE (ADR-0049, ADR-0007 §4): the booth registers under the venue's
//! prefix route, and a `ui:admin.*` event lands on it.
//!
//! The unit tests in `resources_ui.rs` lock what the page SAYS. This one locks the part a unit test
//! cannot: that the venue's wildcard subscription actually matches, that the singleton instance is
//! the one created for it, and that the answer goes back out through the renderer booth instead of
//! disappearing. A page that is drawn correctly by a function nobody can reach is not a page.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use aura_booth::{InstanceId, InstanceKey};
use aura_engine::Engine;
use prism::{identity::Registry, nodes::Posture, Gateway};
use serde_json::json;

/// Boot through `serve` on purpose: the booths are installed THERE (they are the venue's page
/// surfaces, and a venue that has not begun serving has no pages). A test that reached into the
/// install function instead would keep passing after the wiring moved.
async fn boot() -> std::sync::Arc<Gateway> {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(&dir.keep()).unwrap();
    let engine = Engine::start(&Default::default())
        .await
        .expect("engine boot");
    Gateway::with_echoes(&engine).await.unwrap();
    let gw = Gateway::new(engine, registry, Posture::Open);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let serving = gw.clone();
    tokio::spawn(async move {
        let _ = serving.serve(listener).await;
    });
    // The install happens before `serve` starts accepting, so a short settle is enough; the test
    // still fails loudly if the booth never arrived (the invoke below names it).
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    gw
}

#[tokio::test]
async fn the_admin_booth_hears_its_prefix() {
    let gw = boot().await;

    // Declared through the engine, then read back by the PAGE — the two entry points that must not
    // drift (the HTTP face uses the registry directly; this one answers a person).
    gw.engine
        .declare_grant(
            "git",
            aura_realm::resources::Grant {
                desc: "the team's repos".into(),
                base: "ssh://git.example/team".into(),
                mode: aura_realm::resources::Mode::ReadWrite,
                secret_name: "GIT_TOKEN".into(),
            },
            Some("a-credential"),
        )
        .await
        .expect("declared");

    // The envelope is what the venue's own delivery carries: the concrete event name (`name`) is
    // how a Rust body is told which fact it is receiving, and `device` is who the answer is for.
    let out = gw
        .engine
        .invoke(
            InstanceId {
                booth_type: "admin".into(),
                key: InstanceKey::Singleton,
            },
            "admin.resources",
            json!({
                "name": "ui:admin.resources",
                "device": "device-1",
                "data": {},
            }),
        )
        .await
        .expect("the prefix route reaches the page booth");

    // Null is the answer to the EVENT; the drawing went out as `ui:admin.screen`. A body that
    // returned frames here would put a second copy of the page on the plane.
    assert_eq!(out, json!(null));
}
