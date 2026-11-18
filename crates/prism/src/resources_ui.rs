//! The grant surface as a PAGE (ADR-0007 §4 "management is a view"; ADR-0049).
//!
//! Why the venue draws this rather than an application: a grant belongs to the plane, not to any
//! booth — no application can list what the venue offers, and none should be able to. The page is
//! therefore the plane's own, in the same sense the sign-in screen is (`auth.rs`): the venue is an
//! emitter on the renderer's wire like any other, and the renderer only draws.
//!
//! Shape of the exchange, per `fluxen` interaction-patterns §1–2: a page event (`admin.resources`)
//! is answered with ONE action list — `create` the page, and `set` the list slot whenever the rows
//! change. `set` and not `append`/`remove`: the list is written whole because it is short and the
//! alternative is a renderer that has to trust ids it did not mint.
//!
//! The credential never travels back out: the row shows the NAME of the key (`secret_name`), which
//! is all the venue keeps in the grant row, and the key itself lives in a separate table that no
//! read here touches.

use std::sync::Arc;

use aura_realm::resources::{Grant, Registry};
use serde_json::{json, Value};

/// The page's own events. Everything the venue exposes to a page is `ui:`-prefixed (one marker, one
/// direction), and the management verbs carry `admin.` so the prefix route can be a wildcard.
pub const ADMIN_PREFIX: &str = "ui:admin.";
pub const RESOURCES_EVENT: &str = "ui:admin.resources";
pub const DECLARE_EVENT: &str = "ui:admin.declare";
pub const REVOKE_EVENT: &str = "ui:admin.revoke";
/// Where the answer goes: the page's frames, addressed to the device that asked.
const SCREEN_EVENT: &str = "ui:admin.screen";
/// The venue's "this connection is on island X" broadcast. Every region hears it; this booth draws
/// only when the island is its own — which is what makes the page appear when the tab is opened,
/// lazily, instead of at connect time for everyone.
const CONNECT_EVENT: &str = "ui:connect";

/// The booth type. One instance: the plane is one venue, and a grant is a row in it, not a
/// per-client view of one.
const BOOTH: &str = "admin";

/// The slot the page declares for its list. A slot name is a sentence the layout's author wrote —
/// this file is that author, and `set_list` is the only writer.
const LIST_SLOT: &str = "grants";

pub(crate) async fn install(engine: &aura_engine::Engine) {
    let resources = engine.realm.lock().await.resources.clone();
    let booth = aura_booth::BoothType {
        name: BOOTH.to_string(),
        body: aura_booth::Body::Rust(Arc::new(move |ctx, args| {
            let resources = resources.clone();
            let ctx = ctx.clone();
            Box::pin(async move {
                let ops = match answer(&resources, &args) {
                    Ok(ops) => ops,
                    Err(e) => {
                        eprintln!("prism: admin page: {e}");
                        // A failure is still an ANSWER: a page that asked and hears nothing cannot
                        // tell a broken venue from a slow one.
                        vec![notice(&format!("{e}"))]
                    }
                };
                let Some(device) = args.get("device").and_then(Value::as_str) else {
                    return Ok(Value::Null);
                };
                ctx.emit(
                    SCREEN_EVENT,
                    json!({ "name": SCREEN_EVENT, "device": device, "data": ops }),
                )
                .await?;
                Ok(Value::Null)
            })
        })),
        idle_ttl: None,
        max_exec: None,
        hooks: std::collections::BTreeMap::new(),
        receives: vec![
            aura_booth::ReceiveDecl {
                event: format!("{ADMIN_PREFIX}*"),
                entry: None,
                resolution: aura_booth::RouteResolution::Singleton,
                wildcard: true,
                // A grant is a permission: reading it, granting it and withdrawing it are the
                // plane's admin face, so an anonymous connection is refused before delivery (the
                // venue checks the declaration, not the booth).
                auth: true,
            },
            // WHERE THE PERSON WALKED, in the same broadcast every region gets. Without this the
            // page exists in the navigation and nowhere else: a tab that opens onto nothing.
            aura_booth::ReceiveDecl {
                event: CONNECT_EVENT.to_string(),
                entry: None,
                resolution: aura_booth::RouteResolution::Singleton,
                wildcard: false,
                auth: false,
            },
        ],
        concurrency: aura_booth::Concurrency::Queued,
    };
    match engine.register(booth).await {
        Ok(()) => println!("prism: `{ADMIN_PREFIX}<verb>` reaches the grant page (booth `{BOOTH}`)"),
        Err(e) => eprintln!("prism: the grant page booth did not register: {e}"),
    }
}

/// One delivery in, one action list out. Pure in the sense that matters: the only thing it touches
/// is the venue's own registry, so the page can never disagree with the table it draws.
fn answer(resources: &Registry, envelope: &Value) -> anyhow::Result<Vec<Value>> {
    let event = envelope.get("name").and_then(Value::as_str).unwrap_or("");
    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    let text = |name: &str| -> anyhow::Result<String> {
        data.get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("`{name}` must be a string"))
    };
    Ok(match event {
        // THE PAGE APPEARS WHEN ITS TAB IS OPENED. `connect` carries the island the renderer
        // reported; the other islands' booths hear the same broadcast and answer nothing.
        CONNECT_EVENT => {
            if envelope.get("island").and_then(Value::as_str) != Some(BOOTH) {
                return Ok(Vec::new());
            }
            let rows = rows(resources)?;
            vec![create_page(), set_list(&rows)]
        }
        RESOURCES_EVENT => {
            // The page AND its rows in one list: a `create` alone would draw an empty table and
            // wait for a second frame that nothing sends.
            let rows = rows(resources)?;
            vec![create_page(), set_list(&rows)]
        }
        DECLARE_EVENT => {
            let kind = text("kind")?;
            // The same door rule `/admin/resources` applies: a kind no node can move is refused
            // here too, or this page becomes the way around the refusal. The credential is checked
            // against the shape its kind needs, once, before anything is stored.
            let secret = data
                .get("secret")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty());
            crate::admin::declaration_shape(&kind, secret).map_err(anyhow::Error::msg)?;
            let mode = match data.get("mode").and_then(Value::as_str) {
                Some(m) => aura_realm::resources::Mode::parse(m)?,
                None => aura_realm::resources::Mode::ReadOnly,
            };
            let grant = Grant {
                desc: data
                    .get("desc")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                base: text("base")?,
                mode,
                secret_name: data
                    .get("secret_name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            };
            resources.declare(&kind, &grant, crate::identity::now_ms())?;
            // The key is handed in as a VALUE once and lives in its own table; the page never sees
            // it again, which is why the row can only ever show `secret_name`.
            if let Some(value) = secret {
                resources.secret(&kind, value)?;
            }
            vec![set_list(&rows(resources)?), notice(&format!("declared `{kind}`"))]
        }
        REVOKE_EVENT => {
            let kind = text("kind")?;
            resources.revoke(&kind)?;
            vec![set_list(&rows(resources)?), notice(&format!("withdrew `{kind}`"))]
        }
        other => {
            eprintln!("prism: admin page heard `{other}` and has no answer for it");
            Vec::new()
        }
    })
}

/// The rows, in the store's own shape: whatever `grant()` returns is what the page shows, so the
/// management view cannot drift from the table by re-typing its fields.
fn rows(resources: &Registry) -> anyhow::Result<Vec<(String, Value)>> {
    resources.declared()
}

/// The page: a title, the two forms, the list, and a refresh control (a form with no fields —
/// fluxen renders a bare button as nothing, so every control is a form's submit).
fn create_page() -> Value {
    json!({
        "action": "create",
        "slot": "",
        "data": {
            "type": "frame",
            "attrs": { "class": ["main", "col", "gap", "scrolly"] },
            "children": [
                text("resources"),
                text("what this venue offers as durable data (ADR-0049): a grant is a row here, and a script reaches it through ctx.resource — never by being written one."),
                declare_form(),
                revoke_form(),
                json!({
                    "type": "frame",
                    "attrs": { "class": ["col", "gap"] },
                    "bind": { "value": { "kind": "source", "source": LIST_SLOT } },
                    "children": [],
                }),
                refresh_form(),
            ],
        },
    })
}

/// The list, written whole. `set` over `append`/`remove` because the list is short and a renderer
/// that has to track ids the venue minted is a second source of truth.
fn set_list(rows: &[(String, Value)]) -> Value {
    json!({
        "action": "set",
        "slot": LIST_SLOT,
        "data": {
            "type": "frame",
            "attrs": { "class": ["col", "gap"] },
            "children": rows.iter().map(|(kind, grant)| row(kind, grant)).collect::<Vec<_>>(),
        },
    })
}

/// One grant, as a line a person can read. The mode is the row's own number (`Mode`'s index), and
/// the description is last because it is the longest field and the least load-bearing.
fn row(kind: &str, grant: &Value) -> Value {
    let base = grant.get("base").and_then(Value::as_str).unwrap_or("");
    let mode = match grant.get("mode").and_then(Value::as_u64) {
        Some(1) => "rw",
        _ => "ro",
    };
    let name = grant
        .get("secret_name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty());
    let desc = grant.get("desc").and_then(Value::as_str).unwrap_or("");
    // The KEY is named, never shown: the value lives in its own table and no read here touches it.
    let key = name.map(|n| format!("  key: {n}")).unwrap_or_default();
    text(&format!("{kind}  [{mode}]  {base}{key}  — {desc}"))
}

fn declare_form() -> Value {
    form(
        "admin.declare",
        vec![
            text("declare a grant"),
            input("kind", "text", "kind (git | object)"),
            input("base", "text", "where the bytes are"),
            input("mode", "text", "ro or rw"),
            input("desc", "text", "what a person would call it"),
            input("secret_name", "text", "the key's name (e.g. GIT_TOKEN)"),
            input("secret", "password", "the key itself — handed in once"),
            button("declare"),
        ],
    )
}

fn revoke_form() -> Value {
    form(
        "admin.revoke",
        vec![
            text("withdraw"),
            input("kind", "text", "kind to withdraw"),
            button("withdraw"),
        ],
    )
}

fn refresh_form() -> Value {
    form("admin.resources", vec![button("refresh")])
}

fn text(body: &str) -> Value {
    json!({ "type": "text", "bind": { "value": { "kind": "default", "default": body } } })
}

fn notice(body: &str) -> Value {
    json!({ "action": "set", "slot": "notice", "data": text(body) })
}

/// One form field. `kind` is the INPUT type (`text` | `password`) — a form's fields become the
/// uplink payload, keyed by `field`, which is what `answer` reads back.
fn input(field: &str, kind: &str, hint: &str) -> Value {
    json!({
        "type": "input",
        "bind": { "value": { "kind": "field", "field": field, "type": kind, "default": hint } },
    })
}

/// The submit control: a button IS a form's submit, so a form with no fields still needs one.
fn button(label: &str) -> Value {
    json!({ "type": "button", "attrs": { "oneshot": true }, "bind": { "value": { "kind": "submit", "default": label } } })
}

/// One form. `dest` is the BARE event name the page submits to (the venue adds the `ui:` marker on
/// the way in) — the same rule `auth.rs` follows, so a page never speaks a plane-qualified name.
fn form(dest: &str, children: Vec<Value>) -> Value {
    json!({
        "type": "form",
        "attrs": { "class": ["box", "border", "round", "shadow", "col"] },
        "bind": { "value": { "kind": "dest", "dest": dest } },
        "children": children,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use aura_realm::mq::MqStore;

    fn registry() -> Registry {
        Registry::new(MqStore::mem()).expect("a mem store takes the schemas")
    }

    fn envelope(event: &str, data: Value) -> Value {
        json!({ "name": event, "device": "d1", "data": data })
    }

    /// The page and its rows arrive TOGETHER: a create without the rows draws an empty table and
    /// waits for a frame nothing sends, which is the shape of a page that looks broken.
    #[test]
    fn the_page_carries_its_rows() {
        let ops = answer(&registry(), &envelope(RESOURCES_EVENT, json!({}))).unwrap();
        assert_eq!(ops.len(), 2, "create + the list, one answer");
        assert_eq!(ops[0]["action"], "create");
        assert_eq!(ops[1]["action"], "set");
        assert_eq!(ops[1]["slot"], LIST_SLOT);
        assert_eq!(
            ops[1]["data"]["children"].as_array().unwrap().len(),
            0,
            "nothing declared, an empty list — not a missing one"
        );
    }

    /// Declare → the row is IN the answer that reports it, so the page cannot show a table that
    /// disagrees with the venue's own.
    #[test]
    fn a_declaration_answers_with_the_table_it_changed() {
        let registry = registry();
        let ops = answer(
            &registry,
            &envelope(
                DECLARE_EVENT,
                json!({
                    "kind": "git",
                    "base": "ssh://git.example/team",
                    "mode": "rw",
                    "desc": "the team's repos",
                    "secret_name": "GIT_TOKEN",
                    "secret": "a-credential",
                }),
            ),
        )
        .unwrap();
        // The list comes first, the notice second: the drawing answers before the comment on it.
        let rows = ops[0]["data"]["children"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        let line = rows[0].to_string();
        assert!(line.contains("git"), "{line}");
        assert!(line.contains("[rw]"), "the mode is rendered as the row's own: {line}");
        assert!(line.contains("GIT_TOKEN"), "the KEY IS NAMED: {line}");
        assert!(
            !line.contains("a-credential"),
            "the key itself never comes back out: {line}"
        );

        // And the store agrees: one row, mode rw.
        let grant = registry.grant("git").unwrap().expect("declared");
        assert_eq!(grant["mode"], 1);

        let ops = answer(&registry, &envelope(REVOKE_EVENT, json!({ "kind": "git" }))).unwrap();
        assert_eq!(
            ops[0]["data"]["children"].as_array().unwrap().len(),
            0,
            "withdrawn means gone from the table"
        );
        assert!(registry.grant("git").unwrap().is_none());
    }

    /// A form that names no kind is a mistake the person can read, not a silent no-op.
    #[test]
    fn a_form_without_a_kind_says_so() {
        let err = answer(&registry(), &envelope(REVOKE_EVENT, json!({}))).unwrap_err();
        assert!(err.to_string().contains("kind"), "{err}");
    }

    /// Opening the tab is the trigger: `connect` naming this island paints the page, and the same
    /// broadcast naming another island paints nothing at all.
    #[test]
    fn the_page_appears_when_its_island_is_opened() {
        let registry = registry();
        let mine = json!({ "name": CONNECT_EVENT, "device": "d", "island": BOOTH, "data": {} });
        let ops = answer(&registry, &mine).unwrap();
        assert_eq!(ops.len(), 2, "create + the list, one answer: {ops:?}");
        assert_eq!(ops[0]["action"], "create");

        let elsewhere = json!({ "name": CONNECT_EVENT, "device": "d", "island": "gravity-model", "data": {} });
        assert!(
            answer(&registry, &elsewhere).unwrap().is_empty(),
            "the same broadcast for another island is not this page's to draw"
        );
    }

    /// The page is the other door onto the same table, so it applies the same rule: a kind no node
    /// can move is refused here, and the refusal names it. Without this the page would be the way
    /// around the API's check.
    #[test]
    fn the_page_refuses_a_kind_no_node_can_move() {
        let registry = registry();
        let err = answer(
            &registry,
            &envelope(
                DECLARE_EVENT,
                json!({ "kind": "ftp", "base": "ssh://git.example/team", "mode": "ro" }),
            ),
        )
        .unwrap_err();
        assert!(err.to_string().contains("ftp"), "{err}");
        assert!(
            registry.declared().unwrap().is_empty(),
            "a refused declaration leaves no row"
        );
    }
}
