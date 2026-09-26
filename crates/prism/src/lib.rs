//! prism — the connection plane (ADR-0017 §1/§4), first landing: a WS
//! gateway hosting the Aura echo actor in every embedded language.
//!
//! Wire shape (the reduced echo scope, documented in README):
//! - `GET /ws`, `?protocol=json` selects the debug codec (absent = CBOR,
//!   fixed for the connection lifetime — ADR-0017 §4); `?device=<id>`
//!   echoes a known device anchor (§2 restore path).
//! - ONE frame shape both directions: `{ev, args}`; the protocol does
//!   not encode direction, dispatch cannot branch on it.
//! - Every connect answers `{"ev": "connected", "args": {device_id,
//!   user_id|null}}` — the device assignment frame (§2) and the
//!   restored binding, one frame, no separate handshake.
//! - Inbound `{ev: "<type>", args}` → `engine.call` the actor named by
//!   `ev` (instance key `"ws"`, handler = event name — the multi-entry
//!   model). Identity rides the PAYLOAD (§7 amendment): the handler
//!   receives `{"sender": {device, user}, "args": <original>}`; Ctx,
//!   Job, InstanceId stay identity-free. The result returns to the
//!   CALLING connection as `{ev: "<type>.result", args}`; failures are
//!   `{ev: "error"}` VALUES, never a close.
//! - `{"ev": "signup"|"login", args: {username, password, nickname?}}`
//!   — business events on the same socket (§4: login is an ordinary
//!   event), answered `signup.result`/`login.result {user_id}` and
//!   setting the connection's auth field (§3's per-connection field,
//!   one set — never a second collection).
//! - `{ev: "broadcast", args: {to: "all"|"auth", ...}}` — the frame
//!   fans out to every live connection (or the auth-filtered subset);
//!   the selector defaults to "all". Prism is the outbound bridge:
//!   the connection table is the gateway's, actors never address
//!   connections (modeling.md).
//!
//! `/admin` upload, `/probe/<alias>` mount, `/assets` are the rest of
//! ADR-0017 — deliberately not here; README states the gap so it reads
//! as scope, not as drift.

pub mod actors;
pub mod identity;

use aura_actor::{call::Waited, InstanceId};
use aura_engine::Engine;
use futures_util::{SinkExt, StreamExt};
use prism_protocol::{Codec, Frame};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::Request;
use tokio_tungstenite::tungstenite::Message;

/// One live connection: its writer channel + its auth field (§3 — the
/// state is PER CONNECTION: a login writes the field, fan-out filters
/// on it; 0 = anonymous). Shared Arc so dispatch can set it while the
/// broadcast pass reads it.
struct Conn {
    tx: mpsc::UnboundedSender<Frame>,
    user: Arc<AtomicU64>,
    device: u64,
}

/// Shared gateway state: the engine, prism's own account registry, and
/// the live-connection table.
pub struct Gateway {
    pub engine: Engine,
    registry: identity::Registry,
    conns: Arc<Mutex<HashMap<u64, Conn>>>,
    next_id: AtomicU64,
}

/// The handshake extras read in accept_hdr (before the upgrade
/// consumes the request): the codec selector and the device echo.
#[derive(Clone)]
struct Handshake {
    codec: Codec,
    device: Option<u64>,
}

impl Gateway {
    pub fn new(engine: Engine, registry: identity::Registry) -> Arc<Self> {
        Arc::new(Self {
            engine,
            registry,
            conns: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
        })
    }

    /// Register the four-language echo actors (steel / python / nushell
    /// embedded sources + the compiled Rust→wasm echo module). A
    /// language carrier absent from this build skips its actor with a
    /// note — the server still boots with the rest (the demo shape
    /// mirrors aura's feature-forwarded tests).
    pub async fn with_echoes(engine: &Engine) -> anyhow::Result<()> {
        let mut registered: Vec<&str> = Vec::new();
        for e in actors::echo_actors() {
            match engine.register(actors::echo_type(&e)).await {
                Ok(()) => registered.push(e.type_name),
                Err(err) => eprintln!("prism: echo [{}] not registered: {err}", e.type_name),
            }
        }
        println!("prism: echo actors live: {registered:?}");
        Ok(())
    }

    /// Serve forever on `listener`: accept, upgrade, drive.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> anyhow::Result<()> {
        loop {
            let (stream, _) = listener.accept().await?;
            let gw = Arc::clone(&self);
            tokio::spawn(async move {
                let _ = gw.handle_conn(stream).await;
            });
        }
    }

    /// Serve one accepted connection to completion.
    async fn handle_conn(self: Arc<Self>, stream: TcpStream) -> anyhow::Result<()> {
        let hs_slot: Arc<Mutex<Option<Handshake>>> = Arc::default();
        let slot = Arc::clone(&hs_slot);
        let ws = tokio_tungstenite::accept_hdr_async(stream, move |req: &Request, resp| {
            let query = req.uri().query().unwrap_or("");
            *slot.lock().unwrap() = Some(Handshake {
                codec: Codec::from_query(query_param(query, "protocol")),
                device: query_param(query, "device").and_then(|v| v.parse().ok()),
            });
            Ok(resp)
        })
        .await?;

        // The callback ran during the handshake; `unwrap_or` only
        // guards a handshake that never reached `accept`.
        let Handshake { codec, device } =
            hs_slot.lock().unwrap().clone().unwrap_or(Handshake { codec: Codec::Cbor, device: None });
        let (mut write, mut read) = ws.split();

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel::<Frame>();

        // §2 identity mount: a real echoed device restores its binding;
        // anything else (absent, unknown, unparseable) is a fresh
        // connect — a guessed id mints a NEW row, gaining nothing.
        let (device, user0) = match device.filter(|d| self.registry.device_user(*d).is_some()) {
            Some(d) => (d, self.registry.device_user(d).unwrap()),
            None => (self.registry.new_device()?, 0),
        };
        let user = Arc::new(AtomicU64::new(user0));
        self.conns.lock().unwrap().insert(
            id,
            Conn { tx: tx.clone(), user: Arc::clone(&user), device },
        );

        // The connect answer: the device assignment frame AND the
        // (possibly restored) binding — one frame, no separate
        // handshake protocol.
        let _ = tx.send(Frame::new(
            "connected",
            serde_json::json!({"device_id": device, "user_id": (user0 != 0).then_some(user0)}),
        ));

        // Outbound pump: the single socket writer — replies and
        // fan-out share it. Removal on ANY exit path lives here (the
        // presence lesson: an aborted task never runs epilogues, so
        // the map cleanup rides the sender's death: the table prunes
        // closed senders on every broadcast, and this remove covers
        // normal/abort exits).
        let conns = Arc::clone(&self.conns);
        let pump = tokio::spawn(async move {
            let mut rx = rx;
            while let Some(frame) = rx.recv().await {
                if write.send(Message::Binary(codec.encode(&frame))).await.is_err() {
                    break;
                }
            }
            conns.lock().unwrap().remove(&id);
        });

        let result = self.inbound(id, &mut read, codec).await;
        pump.abort();
        let _ = pump.await;
        self.conns.lock().unwrap().remove(&id);
        result
    }

    async fn inbound(
        &self,
        id: u64,
        read: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
              + Unpin),
        codec: Codec,
    ) -> anyhow::Result<()> {
        while let Some(msg) = read.next().await {
            let bytes = match msg? {
                Message::Binary(b) => b,
                Message::Text(t) => t.into_bytes(),
                Message::Close(_) => break,
                _ => continue, // ping/pong stay inside tungstenite
            };
            let frame = match codec.decode(&bytes) {
                Ok(f) => f,
                Err(e) => {
                    self.send_to(id, Frame::new("error", serde_json::json!({"message": e.to_string()})));
                    continue;
                }
            };
            self.dispatch(id, frame).await;
        }
        Ok(())
    }

    /// Dispatch rules (the echo scope): business events first
    /// (`signup`/`login` answered by the registry, §2/§4), then
    /// gateway verbs (`ping`, `broadcast`), then the actor invoke with
    /// the sender envelope mounted in args (§7 amendment: identity is
    /// payload, never Ctx).
    async fn dispatch(&self, id: u64, frame: Frame) {
        let Frame { ev, args } = frame;

        match ev.as_str() {
            "signup" | "login" => {
                self.auth_event(id, &ev, args).await;
                return;
            }
            "broadcast" => {
                let to_auth = args.get("to").and_then(|v| v.as_str()) == Some("auth");
                self.broadcast_frame(Frame::new("broadcast", args), to_auth);
                return;
            }
            "ping" => {
                self.send_to(id, Frame::new("ping.result", serde_json::json!({"pong": true})));
                return;
            }
            _ => {}
        }

        let (device, user) = self.identities(id);
        // Per-event auth ENFORCEMENT (ADR-0017 §2): the actor
        // definition's persisted `auth` block declares which handlers
        // require a bound user — presence of the handler name is the
        // rule. The check rides the schema the upload already
        // persisted (no second declaration surface), runs BEFORE the
        // call so the event never reaches the actor from an anonymous
        // sender, and answers with the same `error` VALUE shape as
        // any other failure (the socket stays up).
        if user == 0 && self.handler_requires_auth(&ev).await {
            self.send_to(
                id,
                Frame::new(
                    "error",
                    serde_json::json!({"ev": ev, "message": "authentication required"}),
                ),
            );
            return;
        }
        // The envelope: the handler receives the sender + the original
        // args. Existing echo handlers pass args through — their
        // results now carry the envelope (the ADR's shape, not a
        // compatibility surface).
        let envelope = serde_json::json!({
            "sender": {"device": device, "user": (user != 0).then_some(user)},
            "args": args,
        });
        let target = InstanceId { actor_type: ev.clone(), key: "ws".into() };
        let payload = match self.engine.call(target, &ev, envelope).await {
            Ok(Waited::Done(Ok(v))) => Ok(v),
            Ok(Waited::Done(Err(e))) => Err(e.to_string()),
            // echo actors are hot by construction; Pending = a cold
            // target mounted under a live name (contract violation).
            Ok(Waited::Pending(_)) => Err("cold call not supported on the echo plane".into()),
            Err(e) => Err(e.to_string()),
        };
        match payload {
            Ok(v) => self.send_to(id, Frame::new(format!("{ev}.result"), v)),
            Err(message) => {
                self.send_to(id, Frame::new("error", serde_json::json!({"ev": ev, "message": message})))
            }
        }
    }

    /// signup/login: registry calls, the auth FIELD of this connection
    /// is the write (§3 one set — no move, no second collection).
    /// Both answers are ordinary `<ev>.result` frames; failures are
    /// error values.
    async fn auth_event(&self, id: u64, ev: &str, args: serde_json::Value) {
        let username = args.get("username").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let password = args.get("password").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let device = self.identities(id).0;
        let outcome = if ev == "signup" {
            let nickname = args.get("nickname").and_then(|v| v.as_str()).unwrap_or("");
            self.registry.signup(&username, nickname, &password, device)
        } else {
            self.registry.login(&username, &password, device)
        };
        match outcome {
            Ok(user_id) => {
                if let Some(conn) = self.conns.lock().unwrap().get(&id) {
                    conn.user.store(user_id, Ordering::Relaxed);
                }
                self.send_to(id, Frame::new(format!("{ev}.result"), serde_json::json!({"user_id": user_id})));
            }
            Err(e) => {
                self.send_to(id, Frame::new("error", serde_json::json!({"ev": ev, "message": e.to_string()})))
            }
        }
    }

    /// Does the actor type `ev` declare handler `ev` (the dispatch rule:
    /// type name == handler name on the echo plane) under its
    /// interface_schema `auth` block? The read rides the SAME persisted
    /// copy `ctx.interface_schema` reflects (4.5b) — the upload-time
    /// introspection is the single declaration surface; an absent schema
    /// (Rust-native type, or a script that declares none) means no auth
    /// requirement. An unresolvable declaration is fail-OPEN: a missing
    /// block never silently blocks a public event — the declared-auth
    /// case requires the block to be present.
    async fn handler_requires_auth(&self, ev: &str) -> bool {
        self.engine
            .realm
            .lock()
            .await
            .schema_of(ev)
            .cloned()
            .flatten()
            .and_then(|schema| schema.get("auth")?.get(ev).map(|v| v.as_bool().unwrap_or(true)))
            .unwrap_or(false)
    }

    fn identities(&self, id: u64) -> (u64, u64) {
        let guard = self.conns.lock().unwrap();
        match guard.get(&id) {
            Some(c) => (c.device, c.user.load(Ordering::Relaxed)),
            None => (0, 0),
        }
    }

    /// Queue one frame to every live connection (optionally only the
    /// authenticated ones — §3's predicate over the ONE set); a closed
    /// sender is pruned in the same pass. Sync: an unbounded send
    /// never blocks.
    fn broadcast_frame(&self, frame: Frame, auth_only: bool) {
        if let Ok(mut conns) = self.conns.lock() {
            // Selection is a predicate READ; retain is only for dead
            // senders. (Conflating them evicted anonymous connections
            // on the first auth-only fan-out.)
            for (_, c) in conns.iter() {
                if !auth_only || c.user.load(Ordering::Relaxed) != 0 {
                    let _ = c.tx.send(frame.clone());
                }
            }
            conns.retain(|_, c| !c.tx.is_closed());
        }
    }

    fn send_to(&self, id: u64, frame: Frame) {
        if let Some(c) = self.conns.lock().unwrap().get(&id) {
            let _ = c.tx.send(frame);
        }
    }
}

/// Parse `k=v&k2=v2` query strings.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

#[cfg(test)]
mod tests {
    use super::query_param;

    #[test]
    fn query_parsing() {
        assert_eq!(query_param("protocol=json", "protocol"), Some("json"));
        assert_eq!(query_param("a=1&protocol=json", "protocol"), Some("json"));
        assert_eq!(query_param("a=1", "protocol"), None);
        assert_eq!(query_param("device=42&protocol=json", "device"), Some("42"));
    }
}
