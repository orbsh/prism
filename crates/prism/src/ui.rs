//! The renderer's leg: `/channel` speaks fluxen's wire (ADR-0017 §4/§8 + its 2026-10-29 Update).
//!
//! Two legs, two shapes, and this file is the second one:
//!
//! - `/ws` — the programmatic leg (CLI, peers, tests): `{ev, args}` frames, the shape the
//!   rest of this crate dispatches.
//! - `/channel` — the browser leg: downstream THE ACTION LIST, `Vec<content::Content>` as a
//!   JSON/CBOR array (always an array, one action is a one-element array), upstream
//!   `content::Outflow {dest, id?, data}` — `dest` is the host-side target of the interaction;
//!   `event` is not a word this wire uses (fluxen ADR 0023), and the downlink's `slot` is a
//!   different thing on purpose (one is a page-side position, the other a host-side target). fluxen is pure UI and never adapts to a host, so
//!   the host adapts here — and since fluxen 460d867 the ENVELOPE IS THE HOST'S, not the
//!   wire's: a map on this socket is refused at the renderer's boundary.
//!
//! Which means prism's own verbs are NOT frames any more. They are `inject` operations (fluxen
//! ADR 0011): a remote write into the page's vals plane that is indistinguishable from an
//! in-page emit, so the layout binds it with `kind: local` like any other event. Two slots
//! carry them — [`IDENTITY_SLOT`] (who am I: device, user) and [`NOTICE_SLOT`] (what failed).
//! Everything else is an ordinary application event, dispatched to the booth NAMED BY THE
//! EVENT (the peer leg's rule: type == handler == `event`), and its answer is a frame list —
//! a handler may return a layout, one operation, or a list, and the renderer only draws.
//!
//! The instance key is `Named("ui")`: the renderer's leg is one instance for now. Per-client
//! partitioning arrives with sessions, and that is the line that will change.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use content::codec::ActiveCodec;
use content::{Content, InjectOp, Outflow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as Ws;
use tokio_tungstenite::WebSocketStream;

use crate::code_export::Prefixed;
use crate::Gateway;

/// The renderer's path. Deliberately not `/ws`: the programmatic leg keeps the `{ev, args}`
/// shape, and one path never carries two shapes.
///
/// The name is `/channel` because that is where the renderer's own page already connects
/// (`mount()`'s default, and stage's endpoint) — the host adapts to the renderer, so the
/// bundle needs no configuration and no page of prism's own.
pub const UI_PATH: &str = "/channel";

/// The instance key every application event on this leg lands on.
const UI_INSTANCE: &str = "channel";

/// The slots THIS venue declares for a turn: the rack the conversation appends to, and the
/// region the answer streams into. They are prism's to name (it serves `layout/venue.yaml`);
/// the executor builds frames against whatever it is told.
const UI_RACK: &str = "chat";
const UI_TEXT: &str = "say";

/// Where this leg puts facts about the CONNECTION: `{device, user}` after connect, a `logout`,
/// a `login.result`. A layout binds it with `bind: {value: {kind: local, slot: identity}}`;
/// the event name inside the inject carries which verb it was, so a subscriber that cares can
/// distinguish and one that does not reads the same fields either way.
pub const IDENTITY_SLOT: &str = "identity";

/// Where the venue talks about the CONNECTION's work: refusals, failures, and a turn's end
/// (`{event, detail?}`). One slot for all of it because they are all "things the venue has to
/// say that are not the application's view" — a layout decides whether to show them, and the
/// venue does not invent a display for each kind.
pub const NOTICE_SLOT: &str = "notice";

/// The renderer connection table: id → connection. The venue owns it, the forwarding booth
/// writes into it, and the peer-plane acts (revocation) reach it.
pub(crate) type UiConns = Arc<std::sync::Mutex<std::collections::HashMap<u64, UiConn>>>;

/// The booth type the venue registers for itself: the application emits, this forwards.
pub const RENDERER_BOOTH: &str = "renderer";

/// The marker on every UI-facing name, both directions (ADR 0023's rule applied here): a page
/// may only ever address, and be addressed by, `view:<name>`. Anything else is internal.
const VIEW_PREFIX: &str = "view:";

/// One renderer connection: its writer channel and the auth field a `login` writes. The
/// table lives on [`Gateway`] so the acts that must reach every leg (revocation) see it.
pub(crate) struct UiConn {
    /// One message = one action list. Batched because the renderer decodes a list per frame:
    /// a layout of twelve operations is one message, not twelve.
    pub tx: mpsc::UnboundedSender<Vec<Content<Value>>>,
    pub device: u64,
    pub user: Arc<AtomicU64>,
    /// How many turns this connection has started — the counter behind per-turn slot names.
    pub turns: u64,
}

/// Install the venue's one subscription: what the application emits as a VIEW event is
/// pushed to the renderers. A Rust body because the venue is IN this process — the frames go
/// to sockets it owns, not over a carrier, and no script has to learn about sockets.
pub(crate) async fn install(engine: &aura_engine::Engine, conns: UiConns) {
    let handler_conns = Arc::clone(&conns);
    let booth = aura_booth::BoothType {
        name: RENDERER_BOOTH.to_string(),
        body: aura_booth::Body::Rust(Arc::new(move |_ctx, args| {
            let conns = Arc::clone(&handler_conns);
            Box::pin(async move {
                forward(&conns, args);
                Ok(Value::Null)
            })
        })),
        idle_ttl: None,
        max_exec: None,
        hooks: std::collections::BTreeMap::new(),
        // Singleton: the venue is one process, not one per client. A view event names the
        // renderer in its payload when it cares (`device`), and the fan-out happens here.
        // ONE PREFIX ROUTE. Every UI-facing emit is `view:<name>` — the marker is what stops a
        // page (or any transport) from forging an internal event — so the venue subscribes to
        // the prefix, not to a list of names it would have to know in advance.
        receives: vec![aura_booth::ReceiveDecl {
            // WITH the star: aura's consumer decides "wildcard" by `event.ends_with('*')`
            // (`realm/src/instance.rs`), and a declaration without it would bind an exact queue
            // named `view:` that nothing ever publishes to — the route matches, the delivery
            // silently never happens.
            event: format!("{VIEW_PREFIX}*"),
            resolution: aura_booth::RouteResolution::Singleton,
            wildcard: true,
        }],
    };
    match engine.register(booth).await {
        Ok(()) => println!("prism: `{VIEW_PREFIX}<name>` reaches renderers (booth `{RENDERER_BOOTH}`, prefix route)"),
        Err(e) => eprintln!("prism: the renderer booth did not register: {e} — the UI will not update"),
    }
}

/// Put an application's frames on the renderers it names. Payload: `{device?: n, frames:
/// <frame or list>}`; `content` is accepted as a synonym (it is the wire's name for it), and
/// a bare frame stands alone. The frames are the renderer's vocabulary — the venue does not
/// interpret them, which is why it can forward a layout, an append and a patch alike.
pub(crate) fn forward(conns: &UiConns, payload: Value) {
    // THE ENVELOPE CARRIES THE ROUTE. A Rust body is not told which event fired (the concrete
    // name is consumed by the dispatcher), so the emitter states it: `name` is the recipient
    // event, marked with the same `view:` prefix as the emit itself. The venue strips the
    // marker — the page is not a party to this vocabulary — and forwards the payload.
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        eprintln!("prism: view event without a `name`: nothing to deliver — {payload}");
        return;
    };
    let name = name.strip_prefix(VIEW_PREFIX).unwrap_or(name).to_string();
    // Who it is for: the channel, or a single device. `sender` is excluded from the fan-out —
    // a page does not hear its own input echoed back.
    let target = payload
        .get("channel_id")
        .or_else(|| payload.get("device"))
        .and_then(Value::as_u64);
    let sender = payload
        .get("sender")
        .and_then(|s| s.get("device").and_then(Value::as_u64).or_else(|| s.as_u64()));
    // What it is: an operation list is DRAWN; anything else is DATA and takes the vals plane,
    // injected into the slot named by the event. Both shapes are legal on this wire and the
    // renderer decides what it can do with them — a host that pre-validated would be teaching
    // the page a second vocabulary.
    let body = payload.get("data").cloned().unwrap_or_else(|| {
        let mut rest = payload.clone();
        if let Some(o) = rest.as_object_mut() {
            for k in ["name", "channel_id", "device", "sender"] {
                o.remove(k);
            }
        }
        rest
    });
    let ops: Vec<Content<Value>> = match body {
        Value::Array(frames) => ops_of(frames, "view"),
        other => inject(&name, &name, other),
    };
    if ops.is_empty() {
        eprintln!("prism: view event `{name}` carried nothing deliverable");
        return;
    }
    let conns = conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut sent = 0usize;
    for c in conns.values() {
        if target.is_some_and(|t| t != c.device) || sender == Some(c.device) {
            continue;
        }
        let _ = c.tx.send(ops.clone());
        sent += 1;
    }
    if sent == 0 {
        eprintln!(
            "prism: view event `{name}` reached no renderer (channel {target:?}, sender {sender:?})"
        );
    }
}

/// Serve one renderer connection to completion.
pub(crate) async fn serve(
    gw: Arc<Gateway>,
    ws: WebSocketStream<Prefixed>,
    codec: ActiveCodec,
    device: u64,
) -> anyhow::Result<()> {
    let (mut write, mut read) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<Content<Value>>>();
    let (id, user) = gw.register_ui(tx, device);

    // The single writer: replies, verbs and a revocation's kick all go through this channel,
    // so the socket has exactly one owner (the peer leg's discipline, on this leg's frames).
    let writer_codec = codec.clone();
    let pump = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match writer_codec.encode(&msg) {
                Ok(bytes) => {
                    if write.send(Ws::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                Err(e) => eprintln!("prism: /ui encode failed: {e}"),
            }
        }
        // Channel closed (the table dropped our sender — revocation or disconnect): close
        // the socket properly so the page SEES it instead of hanging on a dead write half.
        let _ = write.send(Ws::Close(None)).await;
    });

    // The connect answer: identity on the wire, in the renderer's shape. `ev != "draw"`, so
    // a renderer ignores it; a client that cares (a login form, a test) reads `sender`.
    gw.send_ui(id, identify("connected", json!({"device": device, "user": null})));
    eprintln!("prism: renderer leg: device {device} connected (conn {id})");

    // THE LAYOUT IS PUSHED ON CONNECT, not asked for. A client cannot know when it is ready
    // to receive it and the venue cannot know when a page is up, so the venue states what it
    // has and the client draws whatever arrives — one ordering, no handshake, no page code.
    match gw.init_source() {
        Some(crate::init::InitSource::File(frames)) => {
            let ops = ops_of(frames, "init");
            eprintln!("prism: renderer leg: device {device} pushed {} layout frame(s)", ops.len());
            if !ops.is_empty() {
                gw.send_ui(id, ops);
            }
        }
        Some(crate::init::InitSource::Event { event }) => {
            // The venue does not wait for an answer: the application decides what "ready"
            // means, and its view events carry the frames.
            eprintln!("prism: renderer leg: device {device} connect -> emit `{event}`");
            let event = event.clone();
            gw.emit(&event, json!({"device": device})).await;
        }
        None => {
            eprintln!("prism: renderer leg: no init source — device {device} gets an empty venue");
            gw.send_ui(id, notice("init.error", None));
        }
    }

    while let Some(msg) = read.next().await {
        let bytes = match msg? {
            Ws::Binary(b) => b,
            Ws::Text(t) => t.into_bytes(),
            Ws::Close(_) => break,
            _ => continue, // ping/pong stay inside tungstenite
        };
        // The UPLINK is a map (`Outflow`), and fluxen's `decode_auto` now guards the DOWNSTREAM
        // shape by first byte — it refuses a map on purpose ("the envelope belongs to the host"),
        // which would refuse every uplink. So this side decodes with the codec the connection
        // PINNED: a renderer's transport sends in that same codec, and a client that flips its
        // encoder mid-connection is not a case worth a second parser.
        let uplink: Outflow = match codec.decode(&bytes) {
            Ok(v) => v,
            Err(e) => {
                // A frame we cannot read is the client's bug, and its payloads are not ours
                // to guess. Say so on the log, keep the socket up.
                eprintln!("prism: /channel uplink not decodable as Outflow: {e}");
                continue;
            }
        };
        dispatch(&gw, id, &user, device, uplink).await;
    }

    gw.unregister_ui(id);
    let _ = pump.await;
    Ok(())
}

/// One uplink, one answer. Verbs first (they are the plane's own), then the booth named by
/// the event.
async fn dispatch(
    gw: &Arc<Gateway>,
    id: u64,
    user: &Arc<AtomicU64>,
    device: u64,
    uplink: Outflow,
) {
    // The name the page sent. fluxen calls the uplink's destination `slot` (ADR 0023: one word
    // per side, `event` belongs to the DOM and to aura). prism's own word for it is the intent
    // of this connection — local, not on any wire.
    let asked = uplink.dest.clone();
    let data = uplink.data;
    match asked.as_str() {
        "login" | "signup" => {
            let username = data.get("username").and_then(Value::as_str).unwrap_or("");
            let password = data.get("password").and_then(Value::as_str).unwrap_or("");
            let nickname = data.get("nickname").and_then(Value::as_str).unwrap_or("");
            let outcome = if asked == "signup" {
                gw.registry().signup(username, nickname, password, device)
            } else {
                gw.registry().login(username, password, device)
            };
            match outcome {
                Ok(user_id) => {
                    user.store(user_id, Ordering::Relaxed);
                    // The SAME slot `connected` wrote, with the event name saying which verb
                    // it was: a client reads `identity` and always sees who it is.
                    gw.send_ui(id, identify(&format!("{asked}.result"), json!({"device": device, "user": user_id})));
                }
                Err(e) => {
                    eprintln!("prism: /channel {asked} refused: {e}");
                    gw.send_ui(id, notice("error", Some(e.to_string())));
                }
            }
        }
        "logout" => match gw.registry().logout(device) {
            Ok(()) => {
                user.store(0, Ordering::Relaxed);
                gw.send_ui(id, identify("logout.result", json!({"device": device, "user": null})));
            }
            Err(e) => {
                eprintln!("prism: /channel logout refused: {e}");
                gw.send_ui(id, notice("error", Some(e.to_string())));
            }
        },
        "turn.submit" | "turn.cancel" => turn(gw, id, user, device, &asked, data).await,
        other => {
            // The auth gate runs BEFORE the call, reading the booth's persisted
            // interface_schema `auth` block — the same declaration face the peer leg
            // enforces, so a booth declares its requirement once for both legs.
            let bound = user.load(Ordering::Relaxed);
            if bound == 0 && gw.handler_requires_auth(other).await {
                eprintln!("prism: /channel event `{other}` refused: authentication required");
                gw.send_ui(id, notice("error", Some(format!("`{other}` requires a bound user"))));
                return;
            }
            let envelope = json!({
                "sender": {"device": device, "user": (bound != 0).then_some(bound)},
                "args": data,
            });
            let target = aura_booth::InstanceId {
                booth_type: other.to_string(),
                key: aura_booth::InstanceKey::Named(UI_INSTANCE.to_string()),
            };
            match gw.engine.call(target, other, envelope).await {
                // A booth may answer a layout, one operation, or a list — all of it is drawn
                // by the same decoder, so the answer is one action list either way.
                Ok(aura_booth::call::Waited::Done(Ok(v))) => {
                    // AN OPERATION IS DRAWN; ANYTHING ELSE IS DATA, AND DATA HAS NO PATH DOWN
                    // THIS LEG. The venue does not invent a name for it — names belong to the
                    // emitter (a booth that wants the page to see something emits `view:<name>`
                    // with the envelope that says which channel and who is talking). What is
                    // left here is a diagnostic: an answer that draws nothing is a booth bug a
                    // reader would otherwise hunt for in the renderer.
                    let ops = ops_of(vec![v], other);
                    if ops.is_empty() {
                        eprintln!(
                            "prism: /channel event `{other}` answered data, not operations — nothing to draw"
                        );
                    } else {
                        gw.send_ui(id, ops);
                    }
                }
                Ok(aura_booth::call::Waited::Done(Err(e))) => {
                    eprintln!("prism: /channel event `{other}` answered an error: {e}");
                    gw.send_ui(id, notice("error", Some(e.to_string())));
                }
                Ok(aura_booth::call::Waited::Pending(_)) => {
                    eprintln!("prism: /channel event `{other}`: this leg has no cold path");
                    gw.send_ui(id, notice("error", Some("cold path not available here".into())));
                }
                Err(e) => {
                    eprintln!("prism: /channel event `{other}`: {e}");
                    gw.send_ui(id, notice("error", Some(e.to_string())));
                }
            }
        }
    }
}

/// The renderer's turn: the same executor the peer leg drives, asked for VIEW FRAMES. The
/// slots are prism's to declare (it serves the layout), the frames are gravity's to build,
/// and the renderer draws whatever arrives — no delta never reaches this leg.
async fn turn(
    gw: &Arc<Gateway>,
    id: u64,
    user: &Arc<AtomicU64>,
    device: u64,
    event: &str,
    data: Value,
) {
    if event == "turn.cancel" {
        gw.turn_cancel(id);
        return;
    }
    // The turn type declares `turn` user-gated — the same declaration face as everything
    // else on this leg. A renderer may connect anonymously; it may not spend a model call.
    if user.load(Ordering::Relaxed) == 0 {
        eprintln!("prism: /channel turn refused: authentication required");
        gw.send_ui(id, notice("error", Some("a turn requires a bound user".into())));
        return;
    }
    let content = data
        .get("content")
        .or_else(|| data.get("value"))
        .or_else(|| data.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if content.is_empty() {
        eprintln!("prism: /channel turn refused: empty message");
        gw.send_ui(id, notice("error", Some("the turn carried no content".into())));
        return;
    }
    // The conversation's partition: one channel per renderer DEVICE until sessions exist
    // (the executor's instance key is the channel id, so this is what keeps two browsers
    // out of one another's history).
    let session_id = data
        .get("session_id")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| device.to_string());
    let table = gw.turns_arc();
    if table.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_live(id) {
        eprintln!("prism: /channel turn refused: a turn is already live — cancel first");
        gw.send_ui(id, notice("error", Some("a turn is already live on this connection".into())));
        return;
    }
    // The deployment facts (endpoint, model, store) are read per submit: a venue without
    // them still serves the shell, and the refusal names what is missing.
    let config = match crate::turns::TurnConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("prism: /channel turn: {e}");
            gw.send_ui(id, notice("turn.error", Some(e.to_string())));
            return;
        }
    };
    table.lock().unwrap_or_else(std::sync::PoisonError::into_inner).reserve(id);
    let realm = Arc::clone(&gw.engine.realm);
    // ONE REGION PER TURN. A bubble reads the region it was bound to, so a constant name would
    // make turn 2's `set` and patches rewrite the text inside turn 1's bubble — every frame
    // correct, the history smeared. The counter lives with the connection (a reconnect gets a
    // fresh layout, so it has no old bubbles to protect).
    let Some(text) = gw.next_turn_slot(id, UI_TEXT) else {
        eprintln!("prism: /channel turn: connection {id} is already gone");
        return;
    };
    let slots = crate::turns::ViewSlots { rack: UI_RACK.to_string(), text };
    let out = Arc::clone(gw);
    let send = move |ev: &str, a: Value| match ev {
        "turn.frames" => {
            let Some(frames) = a.get("frames").and_then(Value::as_array).cloned() else {
                eprintln!("prism: turn.frames carried no array");
                return;
            };
            // One batch per round: the frames of a round are ordered (a `set` of the text
            // region must precede the patch that fills it), and a batch preserves that.
            let ops = ops_of(frames, "turn");
            if !ops.is_empty() {
                out.send_ui(id, ops);
            }
        }
        "turn.done" => out.send_ui(id, notice("turn.done", None)),
        "turn.error" => {
            eprintln!("prism: turn failed: {a}");
            out.send_ui(id, notice("turn.error", Some(a.to_string())));
        }
        other => eprintln!("prism: the turn answered `{other}` on the renderer leg: {a}"),
    };
    tokio::spawn(crate::turns::run_turn(
        realm, table, config, id, session_id, content, Some(slots), send,
    ));
}

/// A booth's answer, a layout file or a turn's round as an ACTION LIST. A frame that is not an
/// Accrete operation is named and SKIPPED: one typo still draws the rest, and the log is where
/// the fix starts. Nothing is sent when nothing survives — an empty array is a frame that says
/// nothing, which the renderer would decode happily and draw nothing from.
fn ops_of(frames: Vec<Value>, what: &str) -> Vec<Content<Value>> {
    frames
        .into_iter()
        .enumerate()
        .filter_map(|(index, frame)| match serde_json::from_value::<Content<Value>>(frame) {
            Ok(op) => Some(op),
            Err(e) => {
                eprintln!("prism: {what} frame #{index} is not an Accrete operation: {e}");
                None
            }
        })
        .collect()
}

/// The venue speaking about the CONNECTION: `inject` writes the event into the page's vals
/// plane exactly as an in-page emit would (fluxen ADR 0011), which is the only way this leg can
/// say anything — a renderer refuses a frame that is not an action list, and that refusal is
/// deliberate (a map here would be a second protocol).
fn identify(name: &str, data: Value) -> Vec<Content<Value>> {
    inject(IDENTITY_SLOT, name, data)
}

/// One `inject` into a named slot. Every way this leg speaks goes through here, so the shape of
/// "the venue says something" exists once.
fn inject(slot: &str, name: &str, data: Value) -> Vec<Content<Value>> {
    vec![Content::Inject(InjectOp {
        slot: slot.to_string(),
        name: Some(name.to_string()),
        id: None,
        data,
    })]
}

/// A refusal, a failure, or a turn's end — same mechanism, the notice slot. `detail` is the
/// reason the log already carries; a layout may show it, and the venue decides nothing about
/// how a page presents it.
fn notice(name: &str, detail: Option<String>) -> Vec<Content<Value>> {
    let mut data = json!({});
    if let Some(detail) = detail {
        data["detail"] = json!(detail);
    }
    inject(NOTICE_SLOT, name, data)
}
