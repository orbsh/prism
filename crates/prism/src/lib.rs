//! prism — the connection plane (ADR-0017 §1/§4), first landing: a WS
//! gateway hosting the Aura echo booth in every embedded language.
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
//! - Inbound `{ev: "<type>", args}` → `engine.call` the booth named by
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
//! - `{ev: "logout"}` — clears the device's user binding (§2's table,
//!   the cross-reconnect fact) AND the connection's auth field (§3,
//!   immediately — the next auth-gated event errors, the next
//!   broadcast excludes this connection). Idempotent; answered
//!   `logout.result {ok: true}` even when already anonymous.
//! - `{ev: "broadcast", args: {to: "all"|"auth", ...}}` — the frame
//!   fans out to every live connection (or the auth-filtered subset);
//!   the selector defaults to "all". Prism is the outbound bridge:
//!   the connection table is the gateway's, booths never address
//!   connections (modeling.md).
//!
//! `/admin/nodes...` (the ADR-0015 approval surface) and the
//! `/code/{sha256}` export (ADR-0027) ride the same accept loop, as do the
//! renderer's faces (ADR-0017 §8): `/assets/<rel>` (the static bundle,
//! `assets.rs`) and `/ui` (the renderer's leg, `ui.rs` — fluxen's
//! `Message`/`Outflow` wire, with `init` answered from `PRISM_INIT`,
//! `init.rs`). ONE SHAPE PER LEG: `/ws` is the programmatic leg and keeps
//! `{ev, args}`; `/ui` carries the renderer's vocabulary. Booth upload is
//! the rest of ADR-0017 — deliberately not here; README states the gap so
//! it reads as scope, not as drift.


// Unit tests may use the unwrap/panic vocabulary the production targets are
// denied (a panicking test is a failing test). `cfg(test)` is on only for this
// crate's own test build; `tests/` crates set the same line at their own root.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn))]
pub mod assets;
pub mod booths;
pub mod admin;
pub mod code_export;
pub mod effectors;
pub mod identity;
pub mod init;
pub mod mounts;
pub mod nodes;
pub mod turns;
pub mod ui;

use aura_booth::{call::Waited, InstanceId, InstanceKey};
use aura_engine::Engine;
use futures_util::{SinkExt, StreamExt};
use prism_protocol::{Codec, Frame};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
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

/// The venue's optional faces: the renderer's static bundle and the `init` answer's
/// source. Both are DEPLOYMENT FACTS with a legal absence — an install that serves
/// identity/turns/peers and no UI is a real deployment, and it says so at boot.
#[derive(Default)]
pub struct Deployment {
    pub assets: Option<assets::AssetRoot>,
    pub init: Option<init::InitSource>,
}

impl Deployment {
    /// Read both facts from the environment. A declared-but-broken fact is a boot error
    /// (the operator must see it), never a silent downgrade to "no UI".
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self { assets: assets::AssetRoot::from_env()?, init: init::InitSource::from_env()? })
    }

    /// One line per configured face, for the boot log.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(a) = &self.assets {
            lines.push(a.describe());
        }
        if let Some(i) = &self.init {
            lines.push(i.describe());
        }
        lines
    }
}

/// Shared gateway state: the engine, prism's own account registry, the
/// declared trust posture (ADR-0015 §7 — constructed, never defaulted),
/// the two optional front-end facts, and the live-connection table.
pub struct Gateway {
    pub engine: Engine,
    registry: identity::Registry,
    posture: nodes::Posture,
    deployment: Deployment,
    conns: Arc<Mutex<HashMap<u64, Conn>>>,
    next_id: AtomicU64,
    /// Live turn streams, keyed by connection id (turns::TurnTable —
    /// one submit at a time per connection). Shared behind an Arc so
    /// the spawned stream pump can free its slot on completion.
    turns: std::sync::Arc<std::sync::Mutex<turns::TurnTable>>,
    /// The renderer's connections (ADR-0017 §8). A separate table from `conns` because
    /// the two legs write different frame types — but the SAME identity facts: a device,
    /// and the auth field a `login` writes. Keeping them apart is what lets one leg speak
    /// Accrete without teaching the peer leg about it.
    ui_conns: ui::UiConns,
}

/// The handshake extras read in accept_hdr (before the upgrade
/// consumes the request): the codec selector and the device echo.
#[derive(Clone)]
struct Handshake {
    codec: Codec,
    device: Option<u64>,
    /// Which leg this connection belongs to (ADR-0017 §8): `/ui` = the renderer's wire.
    ui: bool,
}

impl Gateway {
    /// Construct with a declared posture (ADR-0015 §7): the mode is an
    /// operator decision the type system refuses to invent — there is
    /// no `Default`, no optional; the binary reads `PRISM_IDENTITY`
    /// (`required` | `open`) and refuses to boot without it.
    pub fn new(engine: Engine, registry: identity::Registry, posture: nodes::Posture) -> Arc<Self> {
        Self::with_deployment(engine, registry, posture, Deployment::default())
    }

    /// Construct with the front-end faces declared (ADR-0017 §1/§8, PLAN Phase 1.10).
    pub fn with_deployment(
        engine: Engine,
        registry: identity::Registry,
        posture: nodes::Posture,
        deployment: Deployment,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            registry,
            posture,
            deployment,
            conns: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            turns: Arc::new(std::sync::Mutex::new(turns::TurnTable::new())),
            ui_conns: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// The node registry over prism's own instance (the approval
    /// surface + the handshake's future verdict source).
    /// The declared trust posture (ADR-0015 §7). Read by the effector mount: the handshake
    /// VERIFIES a signature in both modes, and only `required` consults the registry.
    pub(crate) fn posture(&self) -> nodes::Posture {
        self.posture
    }

    pub fn nodes(&self) -> nodes::NodeStore {
        self.registry.nodes()
    }

    /// Register a renderer connection; returns its id and the auth field the leg writes.
    pub(crate) fn register_ui(
        &self,
        tx: mpsc::UnboundedSender<Vec<content::Content<serde_json::Value>>>,
        device: u64,
        user0: u64,
    ) -> (u64, Arc<AtomicU64>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let user = Arc::new(AtomicU64::new(user0));
        self.ui_conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(
            id,
            ui::UiConn { tx, device, user: Arc::clone(&user), turns: 0 },
        );
        (id, user)
    }

    /// Emit into the default realm, fire and forget. A failure is a LOG, not a reply: the
    /// application's readiness is not the renderer's business (its frames are), and a
    /// connect that dies because an emit failed would be a worse venue than one that says so.
    pub(crate) async fn emit(&self, event: &str, data: serde_json::Value) {
        if let Err(e) = aura_realm::Realm::emit(&self.engine.realm, None, event, data).await {
            eprintln!("prism: emit `{event}` failed: {e}");
        }
    }

    /// Queue one frame to a renderer connection. The TABLE owns the sender: a removed
    /// entry closes the channel, and that is what actually ends the writer (the peer leg's
    /// discipline — a local sender kept across the read loop makes revocation unable to
    /// close anything).
    /// One message = one ACTION LIST (the renderer's wire since fluxen 460d867: no envelope).
    pub(crate) fn send_ui(&self, id: u64, ops: Vec<content::Content<serde_json::Value>>) {
        if let Some(c) = self.ui_conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&id) {
            let _ = c.tx.send(ops);
        }
    }

    /// The streaming region for this connection's NEXT turn — `say.1`, `say.2`, … A constant
    /// name would make each turn patch the region an earlier turn's bubble still reads, so the
    /// counter is per connection and the name is declared fresh every time.
    pub(crate) fn next_turn_slot(&self, id: u64, base: &str) -> Option<String> {
        let mut conns = self.ui_conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let conn = conns.get_mut(&id)?;
        conn.turns += 1;
        Some(format!("{base}.{}", conn.turns))
    }

    pub(crate) fn unregister_ui(&self, id: u64) {
        self.ui_conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
    }

    /// The account/device facts (the same registry the peer leg uses — identity has one home).
    pub(crate) fn registry(&self) -> &identity::Registry {
        &self.registry
    }

    /// The `init` source, if this deployment declared one (`PRISM_INIT`).
    pub(crate) fn init_source(&self) -> Option<init::InitSource> {
        self.deployment.init.clone()
    }

    /// Register the language echo booths (steel / python / nushell
    /// embedded sources). A language carrier absent from this build
    /// skips its booth with a
    /// note — the server still boots with the rest (the demo shape
    /// mirrors aura's feature-forwarded tests).
    pub async fn with_echoes(engine: &Engine) -> anyhow::Result<()> {
        let mut registered: Vec<&str> = Vec::new();
        for e in booths::echo_booths() {
            match engine.register(booths::echo_type(&e)).await {
                Ok(()) => registered.push(e.type_name),
                Err(err) => eprintln!("prism: echo [{}] not registered: {err}", e.type_name),
            }
        }
        println!("prism: echo booths live: {registered:?}");
        // The turn-executor (gravity's real turn + provider booths).
        // Its absence (no python carrier in this build) is a startup
        // note, not a boot failure — turn.submit then answers
        // turn.error per call. The deployment facts (TurnConfig) are
        // read per submit, not here: a boot without them still serves
        // echo/identity; turn verbs report the missing fact.
        match turns::register_executor(engine).await {
            Ok(()) => println!("prism: turn executor live (gravity booths)"),
            Err(err) => eprintln!("prism: turn executor not registered: {err}"),
        }
        Ok(())
    }

    /// Register the declared node mounts (ADR-0001): the deployment manifest
    /// is read from `PRISM_MOUNTS` and replayed at every boot (mount
    /// definitions are not persisted — the manifest is the durable form). An
    /// install with no nodes is legal: this is a no-op then.
    pub async fn with_mounts(engine: &Engine) -> anyhow::Result<()> {
        let mounts = mounts::from_env()?;
        if mounts.is_empty() {
            return Ok(());
        }
        let live = mounts::register_all(engine, &mounts).await?;
        println!("prism: node mounts live: {live:?}");
        Ok(())
    }

    /// Serve forever on `listener`: accept, route, drive. The startup
    /// line states the trust posture where it is visible (ADR-0015 §7's
    /// disclosure rule) — an `open` deployment says so in its own log.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> anyhow::Result<()> {
        // The venue is also a subscriber: an application's view events become renderer
        // frames. Installed here so every path that serves (tests, `main`) gets it.
        ui::install(&self.engine, std::sync::Arc::clone(&self.ui_conns)).await;
        match self.posture {
            nodes::Posture::Required => {
                eprintln!("prism: identity posture: `required` (ed25519 handshake gates effectors — ADR-0015)")
            }
            nodes::Posture::Open => {
                eprintln!(
                    "prism: identity posture: `open` — registrations are unauthenticated, \
                     the network is the boundary (ADR-0015 §7)"
                )
            }
        }
        loop {
            let (stream, _) = listener.accept().await?;
            let gw = Arc::clone(&self);
            tokio::spawn(async move {
                let _ = gw.handle_conn(stream).await;
            });
        }
    }

    /// Serve one accepted connection to completion. The accept loop is
    /// SHARED with the static code export (ADR-0027 §4, PLAN 1.9): the
    /// request head is read raw first; `/code/...` is answered as a
    /// plain HTTP download and the socket closes; every other path
    /// replays the head into the WS handshake (tungstenite 0.24's
    /// callback cannot emit an early non-upgrade response, so the
    /// branch lives below the upgrade machinery, not inside it).
    async fn handle_conn(self: Arc<Self>, mut stream: TcpStream) -> anyhow::Result<()> {
        let Some(head) = code_export::read_head(&mut stream).await else {
            return Ok(()); // client died mid-head: nothing to serve
        };
        // The revocation face needs the WHOLE gateway (registry + live-connection
        // table), not just the node store, and it must be asked FIRST: `serve_admin`
        // claims every path under `/admin` (answering 404 for the ones it does not
        // know), so routing it first made `POST /admin/revoke` unreachable — a dead
        // route behind a live-looking handler (found by the renderer-leg suite).
        if let Some(response) = admin::serve_revoke(self.as_ref(), &head.method, &head.path, &head.body).await {
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        if let Some(response) = admin::serve_dead(self.as_ref(), &head.method, &head.path).await {
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        if let Some(response) = admin::serve_admin(&self.nodes(), &head.method, &head.path, &head.body) {
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
            return Ok(());
        }
        let mq = self.engine.realm.lock().await.mq.clone();
        if code_export::serve_code_export(&mq, &head, &mut stream).await {
            return Ok(());
        }
        // The renderer's static face (ADR-0017 §8): plain downloads on the same accept
        // loop, no upgrade, no event semantics.
        if let Some(assets) = &self.deployment.assets {
            if assets.serve(&head, &mut stream).await {
                return Ok(());
            }
        }
        // Effector dial-in (ADR-0017 §6): `/effector/<alias>` mounts the
        // realm's remote-booth gateway on THIS accept loop. The head is
        // still raw here — the effector's WS carries no `?protocol=`
        // query, so it takes a bare accept over the prefixed stream
        // (the upgrade replaying its head), not the client handshake.
        if let Some(alias) = head.path.strip_prefix("/effector/").filter(|a| !a.is_empty() && !a.contains('/')) {
            let stream = code_export::Prefixed::new(head.bytes, stream);
            let ws = tokio_tungstenite::accept_async(stream).await?;
            return effectors::serve_mounted(self.as_ref(), alias.to_string(), ws).await;
        }
        let stream = code_export::Prefixed::new(head.bytes, stream);

        let hs_slot: Arc<Mutex<Option<Handshake>>> = Arc::default();
        let slot = Arc::clone(&hs_slot);
        let ws = tokio_tungstenite::accept_hdr_async(stream, move |req: &Request, resp| {
            let query = req.uri().query().unwrap_or("");
            // The renderer pins its wire with `codec=`, the programmatic leg with
            // `protocol=`; honour both rather than making one client learn the other's word.
            let codec_q = query_param(query, "protocol").or_else(|| query_param(query, "codec"));
            *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Handshake {
                codec: Codec::from_query(codec_q),
                device: query_param(query, "device").and_then(|v| v.parse().ok()),
                ui: req.uri().path() == ui::UI_PATH,
            });
            Ok(resp)
        })
        .await?;

        // The callback ran during the handshake; `unwrap_or` only
        // guards a handshake that never reached `accept`.
        let Handshake { codec, device, ui } =
            hs_slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone().unwrap_or(Handshake { codec: Codec::Cbor, device: None, ui: false });

        // The renderer's leg (ADR-0017 §8): its own wire, its own writer. It is routed
        // BEFORE the peer leg's connection table: a renderer is not a caller, and its
        // identity/questions land here, not on the booth plane.
        if ui {
            // The renderer's device anchor restores the BINDING, not just the number: the same
            // rule the peer leg follows. A page that reloads keeps its login; a connection whose
            // anchor is a number the registry has never bound starts anonymous either way.
            let (device, user0) = match device {
                Some(d) => match self.registry.device_user(d)? {
                    Some(u) => (d, u),
                    None => (self.registry.new_device()?, 0),
                },
                None => (self.registry.new_device()?, 0),
            };
            let cc = content::codec::ActiveCodec::new(match codec {
                Codec::Json => content::codec::CodecType::Json,
                Codec::Cbor => content::codec::CodecType::Cbor,
            });
            return ui::serve(Arc::clone(&self), ws, cc, device, user0).await;
        }

        let (mut write, mut read) = ws.split();

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel::<Frame>();

        // §2 identity mount: a real echoed device restores its binding;
        // anything else (absent, unknown, unparseable) is a fresh
        // connect — a guessed id mints a NEW row, gaining nothing.
        // One lookup feeds the decision and the value: filtering on
        // `is_some()` and then unwrapping re-asks a question whose answer
        // was already in hand.
        let (device, user0) = match device {
            Some(d) => match self.registry.device_user(d)? {
                Some(u) => (d, u),
                None => (self.registry.new_device()?, 0),
            },
            None => (self.registry.new_device()?, 0),
        };
        let user = Arc::new(AtomicU64::new(user0));
        self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(
            id,
            Conn { tx: tx.clone(), user: Arc::clone(&user), device },
        );

        // The connect answer: the device assignment frame AND the
        // (possibly restored) binding — one frame, no separate
        // handshake protocol. Sent through the map's copy BEFORE the
        // local sender is dropped (below): after this point the map's
        // clone is the ONLY live sender — "the table owns the sender"
        // — so removing the entry (revocation kick) actually closes
        // the channel and the pump sees the shutdown.
        tx.send(Frame::new(
            "connected",
            serde_json::json!({"device_id": device, "user_id": (user0 != 0).then_some(user0)}),
        ))
        .ok();
        drop(tx);

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
            // Channel closed (the table dropped our sender — revocation
            // kick or normal removal): close the socket properly so the
            // client's read side SEES the close instead of hanging on a
            // dropped write half.
            let _ = write.send(Message::Close(None)).await;
            let _ = write.flush().await;
            conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
        });

        let result = self.inbound(id, &mut read, codec).await;
        pump.abort();
        let _ = pump.await;
        self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);
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
    /// gateway verbs (`ping`, `broadcast`), then the booth invoke with
    /// the sender envelope mounted in args (§7 amendment: identity is
    /// payload, never Ctx).
    async fn dispatch(&self, id: u64, frame: Frame) {
        let Frame { ev, args } = frame;

        match ev.as_str() {
            "signup" | "login" => {
                self.auth_event(id, &ev, args).await;
                return;
            }
            "logout" => {
                let device = self.identities(id).0;
                // §3: the per-connection field drives fan-out — clear it
                // so the predicate takes effect on THIS connection
                // immediately; the device-table binding is the
                // cross-reconnect fact and is cleared with it. A refused
                // unbind leaves BOTH facts alone (the row still binds,
                // so the live field must not claim otherwise) and reports
                // itself as an error value — the socket stays up.
                match self.registry.logout(device) {
                    Ok(()) => {
                        if let Some(conn) = self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&id) {
                            conn.user.store(0, Ordering::Relaxed);
                        }
                        self.send_to(id, Frame::new("logout.result", serde_json::json!({"ok": true})));
                    }
                    Err(e) => self.send_to(
                        id,
                        Frame::new("error", serde_json::json!({"ev": "logout", "message": e.to_string()})),
                    ),
                }
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
            "turn.submit" => {
                self.turn_submit(id, args).await;
                return;
            }
            "turn.cancel" => {
                self.turn_cancel(id);
                return;
            }
            _ => {}
        }

        let (device, user) = self.identities(id);
        // Per-event auth ENFORCEMENT (ADR-0017 §2): the booth
        // definition's persisted `auth` block declares which handlers
        // require a bound user — presence of the handler name is the
        // rule. The check rides the schema the upload already
        // persisted (no second declaration surface), runs BEFORE the
        // call so the event never reaches the booth from an anonymous
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
        let target = InstanceId { booth_type: ev.clone(), key: InstanceKey::Named("ws".into()) };
        let payload = match self.engine.call(target, &ev, envelope).await {
            Ok(Waited::Done(Ok(v))) => Ok(v),
            Ok(Waited::Done(Err(e))) => Err(e.to_string()),
            // echo booths are hot by construction; Pending = a cold
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

    /// Server-side revocation (Phase 1 tail): kill a device's binding
    /// and kick its live connections. Not a client event — the entry
    /// is the admin HTTP face (`POST /admin/revoke`), same accept
    /// loop, same curl-first discipline. Three facts move:
    /// the registry row (binding + trust stamp, `Registry::revoke_device`),
    /// each live connection's auth field (cleared so the fan-out
    /// predicate and per-event auth drop it immediately), and the
    /// socket itself (closed with a `revoked` frame — the client
    /// learns WHY it was disconnected). Returns the number of live
    /// connections kicked.
    pub fn revoke_device(&self, device_id: u64) -> anyhow::Result<usize> {
        self.registry.revoke_device(device_id)?;
        let mut kicked = 0;
        let mut dead = Vec::new();
        let mut conns = self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for (id, c) in conns.iter() {
            if c.device == device_id {
                c.user.store(0, Ordering::Relaxed);
                let _ = c.tx.send(Frame::new(
                    "revoked",
                    serde_json::json!({"device_id": device_id, "reason": "revoked by server"}),
                ));
                dead.push(*id);
                kicked += 1;
            }
        }
        // Drop the senders: the channel closes, the pump exits its
        // recv loop, the socket write half drops (the client sees the
        // close after the `revoked` frame), and the pump's own
        // remove-on-exit has nothing left to remove (we dropped the
        // entries here — the pump's cleanup is idempotent).
        for id in dead {
            conns.remove(&id);
        }
        drop(conns);
        // The renderer's leg carries the same auth field, so it is cleared and kicked by the
        // same act: clearing only the peer table would leave a revoked device watching a
        // browser tab that still believes it is logged in.
        let mut ui = self.ui_conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ui_dead = Vec::new();
        for (id, c) in ui.iter() {
            if c.device == device_id {
                c.user.store(0, Ordering::Relaxed);
                ui_dead.push(*id);
                kicked += 1;
            }
        }
        for id in ui_dead {
            ui.remove(&id);
        }
        Ok(kicked)
    }

    /// `turn.submit {session_id, content}` — the streaming turn (the
    /// PLAN's "turn submit ... streaming back" half, riding ADR-0034
    /// iterate). One live turn per connection: a second submit while
    /// one runs is refused (the client cancels or waits). The auth
    /// gate reuses the echo plane's declaration face — the executor
    /// type's persisted interface_schema `auth` block (the stub
    /// declares `turn` user-gated); anonymous senders get the standard
    /// error VALUE. The stream pump runs spawned: the read loop stays
    /// free, deltas flow as `turn.delta` frames, `turn.done` (terminal
    /// value) or `turn.error` closes.
    async fn turn_submit(&self, id: u64, args: serde_json::Value) {
        if self.turns_arc().lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_live(id) {
            self.send_to(
                id,
                Frame::new(
                    "error",
                    serde_json::json!({"ev": "turn.submit", "message": "a turn is already live — cancel first"}),
                ),
            );
            return;
        }
        let (_, user) = self.identities(id);
        if user == 0 && self.type_handler_requires_auth(turns::EXECUTOR_TYPE, "turn").await {
            self.send_to(
                id,
                Frame::new(
                    "error",
                    serde_json::json!({"ev": "turn.submit", "message": "authentication required"}),
                ),
            );
            return;
        }
        let session_id = match args.get("session_id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                self.send_to(
                    id,
                    Frame::new(
                        "error",
                        serde_json::json!({"ev": "turn.submit", "message": "session_id required"}),
                    ),
                );
                return;
            }
        };
        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
        // Reserve SYNCHRONOUSLY — the duplicate check above and this
        // insert are one dispatch pass, no spawn can interleave.
        self.turns_arc().lock().unwrap_or_else(std::sync::PoisonError::into_inner).reserve(id);
        // The deployment facts: read per submit (env). A missing fact
        // reports as turn.error — the session plane serves without it.
        let config = match turns::TurnConfig::from_env() {
            Ok(c) => c,
            Err(e) => {
                self.send_to(
                    id,
                    Frame::new("turn.error", serde_json::json!({"message": e.to_string()})),
                );
                self.turns_arc().lock().unwrap_or_else(std::sync::PoisonError::into_inner).take(id);
                return;
            }
        };
        let conns = Arc::clone(&self.conns);
        let realm = Arc::clone(&self.engine.realm);
        let table = self.turns_arc();
        let send = move |ev: &str, a: serde_json::Value| {
            if let Some(c) = conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&id) {
                let _ = c.tx.send(Frame::new(ev, a));
            }
        };
        tokio::spawn(turns::run_turn(realm, table, config, id, session_id, content, None, send));
    }

    /// `turn.cancel {}` — dispose this connection's live turn. The
    /// turn may still be starting (reserved, stream id unminted):
    /// wait briefly for the realm to mint the id, then dispose
    /// (Dispose is idempotent realm-side — ADR-0034 §3). The pump's
    /// next pull fails ("not live") and reports `turn.error`; the
    /// slot frees. Cancel with no live turn is idempotent-ok
    /// (logout's shape).
    pub(crate) fn turn_cancel(&self, id: u64) {
        let taken = {
            let t = self.turns_arc();
            let mut guard = t.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // A minted id disposes directly; a still-unminted one
            // would have its put rejected by the freed slot — so
            // poll briefly before freeing.
            if guard.live_of(id).is_none() && guard.is_live(id) {
                for _ in 0..50 {
                    drop(guard);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    guard = t.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if guard.live_of(id).is_some() {
                        break;
                    }
                }
            }
            guard.take(id)
        };
        if let Some(stream_id) = taken {
            let realm = Arc::clone(&self.engine.realm);
            tokio::spawn(async move {
                let _ = aura_realm::Realm::iterate(&realm, aura_booth::IterateOp::Dispose { stream_id }).await;
            });
        }
        self.send_to(id, Frame::new("turn.cancel.result", serde_json::json!({"ok": true})));
    }

    /// The turn table behind a shared handle (the spawned pump frees
    /// its slot through this Arc on completion).
    pub(crate) fn turns_arc(&self) -> std::sync::Arc<std::sync::Mutex<turns::TurnTable>> {
        Arc::clone(&self.turns)
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
                if let Some(conn) = self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&id) {
                    conn.user.store(user_id, Ordering::Relaxed);
                }
                self.send_to(id, Frame::new(format!("{ev}.result"), serde_json::json!({"user_id": user_id})));
            }
            Err(e) => {
                self.send_to(id, Frame::new("error", serde_json::json!({"ev": ev, "message": e.to_string()})))
            }
        }
    }

    /// Does the booth type `ev` declare handler `ev` (the dispatch rule:
    /// type name == handler name on the echo plane) under its
    /// interface_schema `auth` block? The read rides the SAME persisted
    /// copy `ctx.interface_schema` reflects (4.5b) — the upload-time
    /// introspection is the single declaration surface; an absent schema
    /// (Rust-native type, or a script that declares none) means no auth
    /// requirement. An unresolvable declaration is fail-OPEN: a missing
    /// block never silently blocks a public event — the declared-auth
    /// case requires the block to be present.
    pub(crate) async fn handler_requires_auth(&self, ev: &str) -> bool {
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

    /// The turn verb's auth gate: the EXECUTOR type's persisted
    /// interface_schema `auth` block, keyed by the handler the submit
    /// drives (`turn`) — the same declaration face the echo plane
    /// enforces, read through the type's schema instead of the
    /// event-name-is-type dispatch rule. Fail-open like the echo gate.
    async fn type_handler_requires_auth(&self, booth_type: &str, handler: &str) -> bool {
        self.engine
            .realm
            .lock()
            .await
            .schema_of(booth_type)
            .cloned()
            .flatten()
            .and_then(|schema| schema.get("auth")?.get(handler).map(|v| v.as_bool().unwrap_or(true)))
            .unwrap_or(false)
    }

    fn identities(&self, id: u64) -> (u64, u64) {
        let guard = self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
        if let Some(c) = self.conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&id) {
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
