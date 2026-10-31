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
//! carry them — `ui:identity` (who am I: device, user) and `ui:notice` (what failed) —
//! because the venue is just another emitter on this wire, not a third vocabulary.
//! Everything else is an ordinary application event, dispatched to the booth NAMED BY THE
//! EVENT (the peer leg's rule: type == handler == `event`), and its answer is a frame list —
//! a handler may return a layout, one operation, or a list, and the renderer only draws.
//!
//! The instance key is `Named("ui")`: the renderer's leg is one instance for now. Per-client
//! partitioning arrives with sessions, and that is the line that will change.

use std::sync::Arc;

use content::codec::ActiveCodec;
use content::{Content, InjectOp, Outflow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as Ws;
use tokio_tungstenite::WebSocketStream;

use crate::code_export::Prefixed;
use crate::identity;
use crate::Gateway;

/// The renderer's path. Deliberately not `/ws`: the programmatic leg keeps the `{ev, args}`
/// shape, and one path never carries two shapes.
///
/// The name is `/channel` because that is where the renderer's own page already connects
/// (`mount()`'s default, and stage's endpoint) — the host adapts to the renderer, so the
/// bundle needs no configuration and no page of prism's own.
pub const UI_PATH: &str = "/channel";

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

/// What the venue says when a connection is established: a FACT about the device, named as it
/// happened. Whoever declared it answers with the layout — the application decides what a fresh
/// device is shown, and there is nothing for a deployment to configure. The `ui:` marker is
/// [`say`]'s job (this is the bare name, like `identity` and `notice`): the venue's own speech
/// goes onto the plane as an envelope, so the prefix route carries it back down and the page can
/// bind the fact that produced the layout.
pub const CONNECT_EVENT: &str = "connect";

/// The renderer connection table: id → connection. The venue owns it, the forwarding booth
/// writes into it, and the peer-plane acts (revocation) reach it.
pub(crate) type UiConns = Arc<std::sync::Mutex<std::collections::HashMap<u64, UiConn>>>;

/// The booth type the venue registers for itself: the application emits, this forwards.
pub const RENDERER_BOOTH: &str = "renderer";

/// The marker on every UI-facing name, both directions (ADR 0023's rule applied here): a page
/// may only ever address, and be addressed by, `ui:<name>`. Anything else is internal.
pub(crate) const UI_PREFIX: &str = "ui:";

/// One renderer connection: its writer channel and the auth field a `login` writes. The
/// table lives on [`Gateway`] so the acts that must reach every leg (revocation) see it.
pub(crate) struct UiConn {
    /// One message = one action list. Batched because the renderer decodes a list per frame:
    /// a layout of twelve operations is one message, not twelve.
    pub tx: mpsc::UnboundedSender<Vec<Content<Value>>>,
    pub device: identity::DeviceId,
    pub user: crate::AuthField,
}

/// Where a booth's DATA payload goes on the programmatic leg: the same event, `{ev, args}`,
/// to the peer connections it addresses. A renderer draws an operation list; a program reads
/// the data shape — one emit, two legs (`gravity ADR-0009`'s two renderings).
pub(crate) type PeerDeliver = Arc<dyn Fn(Option<&str>, Option<&str>, &str, Value) + Send + Sync>;

/// Install the venue's one subscription: what the application emits as a VIEW event is
/// pushed to the renderers (and, as data, to a program). A Rust body because the venue is IN this process — the frames go
/// to sockets it owns, not over a carrier, and no script has to learn about sockets.
pub(crate) async fn install(engine: &aura_engine::Engine, conns: UiConns, peers: PeerDeliver) {
    let handler_conns = Arc::clone(&conns);
    let handler_peers = Arc::clone(&peers);
    let booth = aura_booth::BoothType {
        name: RENDERER_BOOTH.to_string(),
        body: aura_booth::Body::Rust(Arc::new(move |_ctx, args| {
            let conns = Arc::clone(&handler_conns);
            let peers = Arc::clone(&handler_peers);
            Box::pin(async move {
                forward(&conns, &peers, args);
                Ok(Value::Null)
            })
        })),
        idle_ttl: None,
        max_exec: None,
        hooks: std::collections::BTreeMap::new(),
        // Singleton: the venue is one process, not one per client. A view event names the
        // renderer in its payload when it cares (`device`), and the fan-out happens here.
        // ONE PREFIX ROUTE. Every UI-facing emit is `ui:<name>` — the marker is what stops a
        // page (or any transport) from forging an internal event — so the venue subscribes to
        // the prefix, not to a list of names it would have to know in advance.
        receives: vec![aura_booth::ReceiveDecl {
            // WITH the star: aura's consumer decides "wildcard" by `event.ends_with('*')`
            // (`realm/src/instance.rs`), and a declaration without it would bind an exact queue
            // named `ui:` that nothing ever publishes to — the route matches, the delivery
            // silently never happens.
            event: format!("{UI_PREFIX}*"),
            entry: None,
            resolution: aura_booth::RouteResolution::Singleton,
            wildcard: true,
        }],
    };
    match engine.register(booth).await {
        Ok(()) => println!("prism: `{UI_PREFIX}<name>` reaches renderers (booth `{RENDERER_BOOTH}`, prefix route)"),
        Err(e) => eprintln!("prism: the renderer booth did not register: {e} — the UI will not update"),
    }
}

/// Put an application's frames on the renderers it names. Payload: `{device?: n, frames:
/// <frame or list>}`; `content` is accepted as a synonym (it is the wire's name for it), and
/// a bare frame stands alone. The frames are the renderer's vocabulary — the venue does not
/// interpret them, which is why it can forward a layout, an append and a patch alike.
pub(crate) fn forward(conns: &UiConns, peers: &PeerDeliver, payload: Value) {
    // THE ENVELOPE CARRIES THE ROUTE. A Rust body is not told which event fired (the concrete
    // name is consumed by the dispatcher), so the emitter states it: `name` is the recipient
    // event, marked with the same `ui:` prefix as the emit itself. The venue strips the
    // marker — the page is not a party to this vocabulary — and forwards the payload.
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        eprintln!("prism: view event without a `name`: nothing to deliver — {payload}");
        return;
    };
    let name = name.strip_prefix(UI_PREFIX).unwrap_or(name).to_string();
    // Who it is for: the channel, or a single device. `sender` is excluded from the fan-out —
    // a page does not hear its own input echoed back.
    // Addresses arrive as TEXT: a uuid is 16 bytes, and a JSON number cannot carry them (JS
    // Number is f64, so anything past 2^53 loses its tail). The spelling is the codec's business.
    let target = payload
        .get("channel_id")
        .or_else(|| payload.get("device"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let sender = payload
        .get("sender")
        .and_then(|s| s.get("device").and_then(Value::as_str).or_else(|| s.as_str()))
        .map(str::to_string);
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
    // A PROGRAM IS A CLIENT TOO: the data shape (everything that is not an operation list)
    // reaches the peer connections the event addresses, as `{ev, args}` — the same event the
    // renderer gets, minus the drawing.
    if !body.is_array() {
        peers(target.as_deref(), sender.as_deref(), &name, body.clone());
    }
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
        let here = c.device.to_string();
        if target.as_ref().is_some_and(|t| *t != here) || sender.as_ref() == Some(&here) {
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
    device: identity::DeviceId,
    user0: identity::UserId,
    token: identity::TokenId,
) -> anyhow::Result<()> {
    let (mut write, mut read) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<Content<Value>>>();
    let (id, user) = gw.register_ui(tx, device, user0);

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
    say(&gw, device, IDENTITY_EVENT, identity_fact(device, identity::anonymous(), token)).await;
    eprintln!("prism: renderer leg: device {device} connected (conn {id})");

    // THE CONNECTION IS A FACT, AND THE LAYOUT IS THE APPLICATION'S ANSWER TO IT. The venue says
    // `ui:connect {device}` on the plane; whichever booth declared it answers with frames, and
    // the page draws whatever arrives. Nothing is configured here: a deployment has no layout
    // source to name, and a venue with no such booth draws nothing while the connection stays up.
    // Said through `say` like the identity fact, so the envelope names the recipient event: a bare
    // payload is dropped by `forward` (this venue's one speech path).
    eprintln!("prism: renderer leg: device {device} connect -> emit `{UI_PREFIX}{CONNECT_EVENT}`");
    say(&gw, device, CONNECT_EVENT, json!({ "device": device.to_string() })).await;

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
        dispatch(&gw, &user, device, token, uplink).await;
    }

    gw.unregister_ui(id);
    let _ = pump.await;
    Ok(())
}

/// One uplink, one answer. Verbs first (they are the plane's own), then the booth named by
/// the event.
async fn dispatch(
    gw: &Arc<Gateway>,
    user: &crate::AuthField,
    device: identity::DeviceId,
    token: identity::TokenId,
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
                Ok((user_id, fresh)) => {
                    crate::auth_set(user, user_id);
                    // The SAME fact a connect wrote: a page that binds `identity` always sees who
                    // it is — and the ROTATED token, which the page must keep (the old one died).
                    say(&gw, device, IDENTITY_EVENT, identity_fact(device, user_id, fresh)).await;
                }
                Err(e) => {
                    eprintln!("prism: /channel {asked} refused: {e}");
                    say(&gw, device, NOTICE_EVENT, json!({"detail": e.to_string()})).await;
                }
            }
        }
        "logout" => match gw.registry().logout(device) {
            Ok(()) => {
                crate::auth_set(user, identity::anonymous());
                // Logout revokes every token of the device, so the page gets a NEW anonymous one
                // instead of keeping a dead credential in its storage.
                let fresh = match gw.registry().issue_token(device, identity::anonymous()) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("prism: /channel logout: could not issue a fresh token: {e}");
                        token
                    }
                };
                say(&gw, device, IDENTITY_EVENT, identity_fact(device, identity::anonymous(), fresh)).await;
            }
            Err(e) => {
                eprintln!("prism: /channel logout refused: {e}");
                say(&gw, device, NOTICE_EVENT, json!({"detail": e.to_string()})).await;
            }
        },
        other => {
            // THE DECLARATION STILL GATES AT THE VENUE. An anonymous connection never gets to
            // put the event on the plane at all, so a booth that declared `auth` cannot be
            // reached by someone it did not agree to serve — the check stays on this side of
            // delivery, not inside every booth.
            let bound = crate::auth_get(user);
            if identity::is_anonymous(bound) && gw.event_requires_auth(&format!("{UI_PREFIX}{other}")).await {
                eprintln!("prism: /channel event `{other}` refused: authentication required");
                say(&gw, device, NOTICE_EVENT, json!({"detail": format!("`{other}` requires a bound user")})).await;
                return;
            }
            // THE PAGE SENDS A BARE `dest`; THE VENUE PUTS IT ON THE PLANE AS `ui:<dest>`.
            // The marker is the venue's, not the client's: fluxen's uplink stays `Outflow
            // {dest, id?, data}` (different consumers of a renderer make different sense of a
            // bare name), and everything UI-facing on the plane is `ui:`-prefixed.
            //
            // The sender is THIS socket's own identity — a page has no field with which to
            // claim another. Identity is looked up here, once, and travel in the envelope like
            // any other routing fact.
            let event = format!("{UI_PREFIX}{other}");
            let envelope = json!({
                "name": event,
                // The ADDRESS is a device (ADR-0018: a channel is an application's business) and
                // the payload is `data` — the shape a booth reads as a delivery, not a request.
                "device": device.to_string(),
                "sender": {
                    "device": device.to_string(),
                    "user": (!identity::is_anonymous(bound)).then(|| bound.to_string()),
                },
                "data": data,
            });
            gw.emit(&event, envelope).await;
        }
    }
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
/// wants a page to hold is `ui:<fact>`, forwarded by its own renderer booth and landing in the
/// vals slot named by the fact. Two names are conventions, not law: `identity` (who am I:
/// `{device, user}`) and `notice` (what failed: `{detail}`).
async fn say(gw: &Gateway, device: identity::DeviceId, fact: &str, data: Value) {
    let event = format!("{UI_PREFIX}{fact}");
    let envelope = json!({ "name": event, "device": device.to_string(), "data": data });
    gw.emit(&event, envelope).await;
}

/// The identity fact: who this connection is, and the credential that keeps saying so. Ids travel
/// as text; an anonymous connection says so with `null` rather than a nil uuid.
fn identity_fact(
    device: identity::DeviceId,
    user: identity::UserId,
    token: identity::TokenId,
) -> Value {
    json!({
        "device": device.to_string(),
        "user": (!identity::is_anonymous(user)).then(|| user.to_string()),
        "token": token.to_string(),
    })
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


