//! Turn verbs + streaming (Phase 1's remaining half). The gateway is
//! the streaming bridge: a `turn.submit` opens an ADR-0034 iterate
//! stream against the turn-executor booth keyed by session_id, and
//! every pulled envelope becomes a `turn.delta` frame on the calling
//! connection; `turn.done` (carrying the terminal value) or `turn.error`
//! closes the sequence. `turn.cancel` disposes the caller's live stream.
//!
//! Ruling (2026-10-10, replaces the PLAN's "event subscription"
//! wording): the realm's Ctx has no emit — the only landed streaming
//! face is iterate (hot tier). The gateway drives the stream over
//! `Realm::iterate` because the WS connection lives here; the executor
//! booth stays an ordinary realm citizen and the connection plane
//! adapts, it does not bypass. This file carries: the stub executor
//! booth (python generator — the shape gravity's real executor will
//! take, with the real provider riding ctx.iterate inside it), the
//! per-connection live-stream table, and the stream pump.
//!
//! Auth: `turn.submit` requires a bound user. The gate reuses the
//! SAME declaration surface the echo plane enforces — the executor
//! type's persisted interface_schema `auth` block, read by
//! `type_handler_requires_auth` (lib.rs) — no second ACL face.

use aura_booth::{call::CallSlot, Body, BoothType, InstanceId, InstanceKey, IterateOp};
use aura_engine::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aura_realm::Realm;

/// The stub turn executor: a python generator handler yielding text
/// deltas one by one, then returning the terminal value. Stream
/// envelope shape is ADR-0034's, which the pump translates to
/// `turn.delta` / `turn.done` frames. Gravity's real executor replaces
/// the body, not the shape. The `auth` block declares `turn`
/// user-gated — the submit gate reads this persisted schema.
const STUB_SOURCE: &str = r#"
import time

def interface_schema(args):
    return {"auth": {"turn": "required"}}

def turn(args):
    text = args.get("content", "")
    for word in text.split(" "):
        # Pace the stream so a mid-turn cancel (and any client) can
        # actually observe deltas as they happen — a real provider is
        # network-paced; the stub would otherwise drain instantly.
        time.sleep(0.05)
        yield {"delta": word + " "}
"#;

/// The executor booth type name. `turn.submit` targets instance key =
/// session_id (the Phase 6 contract's partition key, honored here as
/// the instance key already).
pub const EXECUTOR_TYPE: &str = "turn_executor";

/// Register the stub executor. A build without the python carrier
/// leaves the type absent — `turn.submit` then answers `turn.error`
/// instead of hanging on an unknown type.
pub async fn register_stub(engine: &Engine) -> anyhow::Result<()> {
    #[cfg(feature = "python")]
    {
        engine
            .register(BoothType {
                name: EXECUTOR_TYPE.into(),
                body: Body::Script {
                    language: "python".into(),
                    source: STUB_SOURCE.into(),
                    encoding: Default::default(),
                },
                idle_ttl: None,
                max_exec: None,
                on_sleep: None,
                on_wake: None,
                receives: Vec::new(),
            })
            .await?;
        Ok(())
    }
    #[cfg(not(feature = "python"))]
    {
        let _ = engine;
        anyhow::bail!("turn executor needs the python carrier")
    }
}

/// Live streams keyed by gateway connection id — one submit at a time
/// per connection. Reserved SYNCHRONOUSLY at submit (a `None` stream id
/// = the stream is still starting; the duplicate-submit check must not
/// depend on the first envelope winning a race). The value fills with
/// the ADR-0034 stream id once the realm mints it, and the slot frees
/// on completion / cancel / error.
#[derive(Default)]
pub struct TurnTable {
    live: HashMap<u64, Option<String>>,
}

impl TurnTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// The stream id already live for this connection, if any.
    pub fn live_of(&self, conn: u64) -> Option<String> {
        self.live.get(&conn).cloned().flatten()
    }

    /// Whether a turn is live (reserved or minted) on this connection.
    pub fn is_live(&self, conn: u64) -> bool {
        self.live.contains_key(&conn)
    }

    /// Reserve the connection's slot at submit time (stream id still
    /// unminted).
    pub fn reserve(&mut self, conn: u64) {
        self.live.insert(conn, None);
    }

    /// Fill in the realm-minted stream id (the first reply arrived).
    /// Refused when the slot was freed meanwhile (cancel raced the
    /// start) — the pump checks the return and abandons.
    pub fn put(&mut self, conn: u64, stream_id: String) -> bool {
        match self.live.get_mut(&conn) {
            Some(slot @ None) => {
                *slot = Some(stream_id);
                true
            }
            _ => false,
        }
    }

    /// Free the connection's slot (completion / cancel / error).
    /// Returns the minted stream id, if it got one.
    pub fn take(&mut self, conn: u64) -> Option<String> {
        self.live.remove(&conn).flatten()
    }
}

/// One pulled stream envelope, classified for the frame translation.
enum Pulled {
    Delta(String),
    Done(Value),
    Malformed(String),
}

/// Classify one ADR-0034 envelope against the turn contract: the
/// carrier wraps every generator `yield` as `item` (done:false), the
/// terminal round (done:true) delivers its `value` as the turn's
/// result.
fn classify(v: &Value, done: bool) -> Pulled {
    if done {
        return Pulled::Done(v.get("value").cloned().unwrap_or(Value::Null));
    }
    let item = v.get("item");
    match item.and_then(|i| i.get("delta")).and_then(|d| d.as_str()) {
        Some(d) => Pulled::Delta(d.to_string()),
        None => Pulled::Malformed("envelope item carries no delta string".into()),
    }
}

/// Free the slot then send one `turn.error` frame through `send` and
/// leave `run_turn` (every failure path in the loop shares this shape;
/// `take` is idempotent — a cancel that freed the slot already is fine).
macro_rules! fail_turn {
    ($table:expr, $conn:expr, $send:expr, $($arg:tt)+) => {{
        $table.lock().unwrap().take($conn);
        $send("turn.error", json!({ "message": $($arg)+ }));
        return;
    }};
}

/// Run one turn to completion on `conn`'s behalf: Start, then Next
/// pulls in a LOOP (no recursion — a long turn must not grow the
/// stack), translating each envelope through `send` until Done /
/// error / malformed. Cancellation frees the slot from the cancel arm
/// (`TurnTable::take`); the pump notices on its next `put` (refused —
/// the stream is disposed and abandoned) or on its next pull failing
/// with "not live". On ANY exit the slot is freed. Dispatch spawns
/// this, so the socket's read loop stays free while deltas flow. The
/// turn args: `session_id` picks the executor instance (the Phase 6
/// partition key), `content` is the turn's input.
pub async fn run_turn(
    realm: aura_realm::SharedRealm,
    table: Arc<Mutex<TurnTable>>,
    conn: u64,
    session_id: String,
    content: String,
    mut send: impl FnMut(&str, Value) + Send + 'static,
) {
    let target = InstanceId {
        booth_type: EXECUTOR_TYPE.into(),
        key: InstanceKey::Named(session_id),
    };
    let mut slot = match Realm::iterate(
        &realm,
        IterateOp::Start {
            target,
            handler: "turn".into(),
            args: json!({"content": content}),
        },
    )
    .await
    {
        Ok(CallSlot::Hot { rx, .. }) => rx,
        Ok(CallSlot::Cold { .. }) => {
            fail_turn!(table, conn, send, "executor is cold — unsupported");
        }
        Err(e) => {
            // Unknown type (no python carrier) or a full queue —
            // reported, never silent.
            fail_turn!(table, conn, send, e.to_string());
        }
    };
    let mut stream_id: Option<String> = None;
    loop {
        let v = match slot.await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => fail_turn!(table, conn, send, e.to_string()),
            Err(_) => {
                // The reply dropped — eviction or a Dispose from cancel.
                fail_turn!(table, conn, send, "stream cancelled or reply dropped");
            }
        };
        let done = v.get("done").and_then(|d| d.as_bool()).unwrap_or(false);
        if stream_id.is_none() && !done {
            // Non-terminal first reply MUST carry the realm-minted id
            // (ADR-0036 §4 — association from done, never a guess).
            let Some(id) = v.get("stream_id").and_then(|s| s.as_str()).map(String::from) else {
                fail_turn!(table, conn, send, "protocol: no stream id on first reply");
            };
            if !table.lock().unwrap().put(conn, id.clone()) {
                // The slot was freed underneath (cancel raced the
                // start): abandon the stream — dispose and stop.
                let _ = Realm::iterate(&realm, IterateOp::Dispose { stream_id: id }).await;
                fail_turn!(table, conn, send, "stream cancelled");
            }
            stream_id = Some(id);
        }
        match classify(&v, done) {
            Pulled::Delta(d) => send("turn.delta", json!({"delta": d})),
            Pulled::Done(value) => {
                send("turn.done", json!({"value": value}));
                table.lock().unwrap().take(conn);
                return;
            }
            Pulled::Malformed(m) => {
                // Kill the stream — a producer violating the shape
                // must not keep pulling.
                if let Some(id) = &stream_id {
                    let _ = Realm::iterate(&realm, IterateOp::Dispose { stream_id: id.clone() }).await;
                }
                fail_turn!(table, conn, send, m);
            }
        }
        // Pull again (done was handled above; the stream stays open).
        let Some(id) = stream_id.clone() else { return };
        slot = match Realm::iterate(&realm, IterateOp::Next { stream_id: id }).await {
            Ok(CallSlot::Hot { rx, .. }) => rx,
            Ok(CallSlot::Cold { .. }) => {
                fail_turn!(table, conn, send, "executor is cold — unsupported");
            }
            Err(e) => fail_turn!(table, conn, send, e.to_string()),
        };
    }
}
