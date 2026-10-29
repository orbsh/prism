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
//! carry them — `view:identity` (who am I: device, user) and `view:notice` (what failed) —
//! because the venue is just another emitter on this wire, not a third vocabulary.
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

/// A turn's end, said as an event so the page can stop its streaming state without knowing a verb.
const TURN_DONE_EVENT: &str = "turn.done";
/// A turn's failure, said the same way.
const TURN_ERROR_EVENT: &str = "turn.error";
const UI_TEXT: &str = "say";

/// Where this leg puts facts about the CONNECTION: `{device, user}` after connect, a `logout`,
/// a `login.result`. A layout binds it with `bind: {value: {kind: local, slot: identity}}`;
/// the event name inside the inject carries which verb it was, so a subscriber that cares can
/// distinguish and one that does not reads the same fields either way.
pub const IDENTITY_EVENT: &str = "identity";

/// Where the venue talks about the CONNECTION's work: refusals, failures, and a turn's end
/// (`{event, detail?}`). One slot for all of it because they are all "things the venue has to
/// say that are not the application's view" — a layout decides whether to show them, and the
/// venue does not invent a display for each kind.
pub const NOTICE_EVENT: &str = "notice";

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
    if payload.get("data").is_none() && payload.get("args").is_some() {
        // A page's own uplink, seen by the venue's subscription to the same prefix. It is a
        // REQUEST (the booth answers by emitting), not a delivery — forwarding it would echo
        // the input back into the page.
        return;
    }
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
    say(&gw, device, IDENTITY_EVENT, json!({"device": device, "user": null})).await;
    eprintln!("prism: renderer leg: device {device} connected (conn {id})");

    // THE LAYOUT IS PUSHED ON CONNECT, not asked for. A client cannot know when it is ready
    // to receive it and the venue cannot know when a page is up, so the venue states what it
    // has and the client draws whatever arrives — one ordering, no handshake, no page code.
    match gw.init_source() {
        Some(crate::init::InitSource::File(frames)) => {
            let ops = ops_of(frames, "init");
            eprintln!("prism: renderer leg: device {device} pushed {} layout frame(s)", ops.len());
            if !ops.is_empty() {
                // Through the same plane as everything else the venue says: the identity fact
                // was emitted first, so the page cannot receive the layout before it — a direct
                // write here raced the queue and could arrive in either order.
                say_ops(&gw, device, "layout", ops).await;
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
            say(&gw, device, NOTICE_EVENT, json!({"detail": "no layout source in this deployment"})).await;
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
                    // The SAME fact a connect wrote: a page that binds `identity` always sees
                    // who it is, so the layout needs no per-verb case.
                    say(&gw, device, IDENTITY_EVENT, json!({"device": device, "user": user_id})).await;
                }
                Err(e) => {
                    eprintln!("prism: /channel {asked} refused: {e}");
                    say(&gw, device, NOTICE_EVENT, json!({"detail": e.to_string()})).await;
                }
            }
        }
        "logout" => match gw.registry().logout(device) {
            Ok(()) => {
                user.store(0, Ordering::Relaxed);
                say(&gw, device, IDENTITY_EVENT, json!({"device": device, "user": null})).await;
            }
            Err(e) => {
                eprintln!("prism: /channel logout refused: {e}");
                say(&gw, device, NOTICE_EVENT, json!({"detail": e.to_string()})).await;
            }
        },
        "turn.submit" | "turn.cancel" => turn(gw, id, user, device, &asked, data).await,
        other => {
            // THE DECLARATION STILL GATES AT THE VENUE. An anonymous connection never gets to
            // put the event on the plane at all, so a booth that declared `auth` cannot be
            // reached by someone it did not agree to serve — the check stays on this side of
            // delivery, not inside every booth.
            let bound = user.load(Ordering::Relaxed);
            if bound == 0 && gw.handler_requires_auth(other).await {
                eprintln!("prism: /channel event `{other}` refused: authentication required");
                say(&gw, device, NOTICE_EVENT, json!({"detail": format!("`{other}` requires a bound user")})).await;
                return;
            }
            // THE UPLINK IS AN INVOKE *FOR NOW*. The ruling is that it becomes an emit of
            // `view:<dest>`, but the marker cannot work until the dispatcher stops handing the
            // body the marked name as its handler name — a script booth's handler is looked up
            // BY that name, and `view:echo_frame` is not a function any script can define (and
            // the miss is silent). See docs/PLAN.md; this is the one place the wire rule waits.
            let envelope = json!({
                "sender": {"device": device, "user": (bound != 0).then_some(bound)},
                "args": data,
            });
            let target = aura_booth::InstanceId {
                booth_type: other.to_string(),
                key: aura_booth::InstanceKey::Named(UI_INSTANCE.to_string()),
            };
            match gw.engine.call(target, other, envelope).await {
                Ok(aura_booth::call::Waited::Done(Ok(v))) => {
                    // An answer that is not an operation draws nothing, and the venue does not
                    // invent a name for it — a booth that wants the page to see something emits
                    // `view:<name>`. Left here is the diagnostic that says so.
                    let ops = ops_of(vec![v], other);
                    if ops.is_empty() {
                        eprintln!(
                            "prism: /channel event `{other}` answered data, not operations — nothing to draw"
                        );
                    } else {
                        gw.send_ui(id, ops);
                    }
                }
                Ok(aura_booth::call::Waited::Done(Err(e))) | Err(e) => {
                    eprintln!("prism: /channel event `{other}` answered an error: {e}");
                    say(&gw, device, NOTICE_EVENT, json!({"detail": e.to_string()})).await;
                }
                Ok(aura_booth::call::Waited::Pending(_)) => {
                    say(&gw, device, NOTICE_EVENT, json!({"detail": "cold path not available here"})).await;
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
        say(&gw, device, NOTICE_EVENT, json!({"detail": "a turn requires a bound user"})).await;
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
        say(&gw, device, NOTICE_EVENT, json!({"detail": "the turn carried no content"})).await;
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
        say(&gw, device, NOTICE_EVENT, json!({"detail": "a turn is already live on this connection"})).await;
        return;
    }
    // The deployment facts (endpoint, model, store) are read per submit: a venue without
    // them still serves the shell, and the refusal names what is missing.
    let config = match crate::turns::TurnConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("prism: /channel turn: {e}");
            say(&gw, device, TURN_ERROR_EVENT, json!({"detail": e.to_string()})).await;
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
        // `send` is a sync callback (the pump drives it), so the emit is spawned: the frames of
        // this turn are already queued on the same writer, which keeps them ahead of this.
        "turn.done" => {
            let out = Arc::clone(&out);
            tokio::spawn(async move { say(&out, device, TURN_DONE_EVENT, json!({})).await });
        }
        "turn.error" => {
            eprintln!("prism: turn failed: {a}");
            let out = Arc::clone(&out);
            let detail = a.to_string();
            tokio::spawn(async move {
                say(&out, device, TURN_ERROR_EVENT, json!({ "detail": detail })).await
            });
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

/// THE VENUE SAYS THINGS AS EVENTS. It is an emitter on this wire like any other: a fact it
/// wants a page to hold is `view:<fact>`, forwarded by its own renderer booth and landing in the
/// vals slot named by the fact. Two names are conventions, not law: `identity` (who am I:
/// `{device, user}`) and `notice` (what failed: `{detail}`).
/// Same path, for a batch of OPERATIONS (the init layout): an action list is drawn, so this is
/// the one shape that skips the vals plane on its way to the page.
async fn say_ops(gw: &Gateway, device: u64, fact: &str, ops: Vec<Content<Value>>) {
    let event = format!("{VIEW_PREFIX}{fact}");
    let envelope = json!({ "name": event, "channel_id": device, "data": ops });
    gw.emit(&event, envelope).await;
}

async fn say(gw: &Gateway, device: u64, fact: &str, data: Value) {
    let event = format!("{VIEW_PREFIX}{fact}");
    let envelope = json!({ "name": event, "channel_id": device, "data": data });
    gw.emit(&event, envelope).await;
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


