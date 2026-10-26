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
//! adapts, it does not bypass. This file carries: the turn-executor
//! registration (gravity's REAL turn booth — the python generator
//! whose `turn` handler fetches the projection from mem-server, pulls
//! tokens from the provider booth, and appends the exchange; prism's
//! test stub had the same shape and is retired), the deployment-facts
//! config, the per-connection live-stream table, and the stream pump.
//!
//! Auth: `turn.submit` requires a bound user. The gate reuses the
//! SAME declaration surface the echo plane enforces — the executor
//! type's persisted interface_schema `auth` block, read by
//! `type_handler_requires_auth` (lib.rs) — no second ACL face.

use aura_booth::{call::CallSlot, InstanceId, InstanceKey, IterateOp};
use aura_engine::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aura_realm::Realm;

/// The turn-executor booth type name (gravity's turn booth; the
/// provider booth it pulls from is registered alongside).
pub const EXECUTOR_TYPE: &str = gravity_booth::TURN_TYPE;

/// The turn submission's deployment facts, declared at boot (env — the
/// same custody discipline as the key itself: names in config, values
/// in the environment where they belong). `key_env` names the env var
/// holding the LLM key — the key VALUE never crosses prism.
#[derive(Clone, Debug)]
pub struct TurnConfig {
    /// mem-server base URL (the memory seam the booth posts through).
    pub store_url: String,
    /// OpenAI-compatible chat-completions endpoint.
    pub endpoint: String,
    /// Model id the endpoint serves.
    pub model: String,
    /// The env var NAME holding the LLM API key.
    pub key_env: String,
    /// How k10r should render the memory it hands back
    /// (`plaintext` / `markdown` / `mermaid`; `GRAVITY_MEMORY_FORMAT`,
    /// default `plaintext`). The executor refuses `struct` — it returns the
    /// fact data for a caller that assembles the graph itself — so prism
    /// passes the value through and gravity raises the error.
    pub memory_format: String,
    /// The k10r namespace the memory TOOLS reach (`GRAVITY_MEMORY_SCOPE`).
    /// Optional by the ruling (gravity ADR-0004 §5): a namespace is k10r's
    /// declaration, so it rides the args as a deployment fact — and an
    /// ABSENT one makes the affected tool fail with the fact named, never
    /// fall back to a default namespace that may belong to someone else.
    pub memory_scope: Option<u64>,
}

impl TurnConfig {
    /// Read from the environment; every field required (a deployment
    /// fact is declared, never defaulted).
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            store_url: env_required("GRAVITY_STORE_URL")?,
            endpoint: env_required("GRAVITY_ENDPOINT")?,
            model: env_required("GRAVITY_MODEL")?,
            key_env: env_required("GRAVITY_KEY_ENV")?,
            memory_format: std::env::var("GRAVITY_MEMORY_FORMAT")
                .unwrap_or_else(|_| "plaintext".to_string()),
            memory_scope: match std::env::var("GRAVITY_MEMORY_SCOPE") {
                Ok(raw) => Some(raw.parse().map_err(|_| {
                    anyhow::anyhow!("GRAVITY_MEMORY_SCOPE `{raw}` is not a namespace id")
                })?),
                Err(_) => None,
            },
        })
    }

    /// The deployment-fact slice of the turn args (the per-turn fields
    /// — channel/member/message — are added by `run_turn`).
    fn as_args(&self) -> Value {
        let mut args = json!({
            "store_url": self.store_url,
            "endpoint": self.endpoint,
            "model": self.model,
            "key_env": self.key_env,
            "memory_format": self.memory_format,
        });
        // Absent stays absent: the tool names the missing fact rather than
        // reading a namespace nobody declared (gravity ADR-0004 §5).
        if let Some(scope) = self.memory_scope {
            args["memory_scope"] = json!(scope);
        }
        args
    }
}

fn env_required(name: &str) -> anyhow::Result<String> {
    std::env::var(name)
        .map_err(|_| anyhow::anyhow!("env var {name} not set (turn deployment fact)"))
}

/// Register gravity's turn + provider booths (the two Rust induction
/// guests — the spawn specs resolve the binaries on deployment; no
/// carrier feature gates this build). Their absence (spec binaries not
/// installed) surfaces at spawn as turn.error per call.
pub async fn register_executor(engine: &Engine) -> anyhow::Result<()> {
    gravity_booth::register(engine).await
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
        $table.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take($conn);
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
/// partition key = the conversation channel), the config supplies the
/// deployment facts.
pub async fn run_turn(
    realm: aura_realm::SharedRealm,
    table: Arc<Mutex<TurnTable>>,
    config: TurnConfig,
    conn: u64,
    session_id: String,
    user_message: String,
    mut send: impl FnMut(&str, Value) + Send + 'static,
) {
    // The instance key IS the channel id (one conversation log per
    // session — the string renders to u64 at the booth, or the session
    // names a channel); member 1 = the agent's projection (mem-core's
    // default member).
    let mut args = config.as_args();
    args["channel_id"] = json!(session_id.parse::<u64>().unwrap_or(1));
    args["member_id"] = json!(1);
    args["user_message"] = json!(user_message);
    let target = InstanceId {
        booth_type: EXECUTOR_TYPE.into(),
        key: InstanceKey::Named(session_id),
    };
    let mut slot = match Realm::iterate(
        &realm,
        IterateOp::Start {
            target,
            handler: "turn".into(),
            args,
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
            if !table.lock().unwrap_or_else(std::sync::PoisonError::into_inner).put(conn, id.clone()) {
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
                table.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take(conn);
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
