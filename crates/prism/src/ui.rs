//! The renderer's leg: `/channel` speaks fluxen's wire (ADR-0017 §4/§10).
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
//! in-page emit, so the layout binds it with `kind: local` like any other event. The venue's own
//! facts ride that same plane — `ui:token` (the credential to keep), `ui:notice` (what failed),
//! `ui:connect` / `ui:login` / `ui:logout` / `ui:disconnect` — because the venue is just another
//! emitter on this wire, not a third vocabulary.
//! Everything else is an ordinary application event, dispatched to the booth NAMED BY THE
//! EVENT (the peer leg's rule: type == handler == `event`), and its answer is a frame list —
//! a handler may return a layout, one operation, or a list, and the renderer only draws.
//!
//! The instance key is `Named("ui")`: the renderer's leg is ONE instance, and it stays one. The
//! wildcard `renderer` booth only ROUTES, and the fan-out already walks one sender per live
//! connection, so a per-client instance would add identity to something that holds no per-client
//! state (PLAN, settled 2026-10-31). A conversation is not this venue's key either: the venue
//! addresses a device, and which conversation that device is in is the application's own row.

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
/// The credential, said when it is issued or rotated. Not "identity": the user is the venue's own
/// answer at every uplink (`resolve` at connect, the connection's auth field after a login), and a
/// page that was told a user would be a second home for a fact that already has one. The token is
/// what the page cannot derive and must keep.
pub const TOKEN_EVENT: &str = "token";

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

/// The rest of the connection's life, said the same way `connect` is (MODELING's rule: a FACT is
/// named as it happened, a REQUEST is imperative). `login` is the device binding to an account
/// (signup included), `logout` the binding ending, `disconnect` the connection going away.
pub const LOGIN_EVENT: &str = "login";
pub const LOGOUT_EVENT: &str = "logout";

/// The account's device list, said to the page that asked: the answer lands in the slot named
/// after the fact, like every other piece of venue speech.
pub const DEVICES_EVENT: &str = "devices";

/// A device was renamed. Display only — the fact carries the device and its new name.
pub const ALIAS_EVENT: &str = "alias";
pub const DISCONNECT_EVENT: &str = "disconnect";

/// The renderer connection table: id → connection. The venue owns it, the forwarding booth
/// writes into it, and the peer-plane acts (revocation) reach it.
pub(crate) type UiConns = Arc<std::sync::Mutex<std::collections::HashMap<u64, UiConn>>>;

/// The booth type the venue registers for itself: the application emits, this forwards.
pub const RENDERER_BOOTH: &str = "renderer";

/// The two slots a FRAME binds, filled by the venue on every connection (ADR-0019).
///
/// They are the frame's whole dynamic surface, and both are facts only the venue holds: who is
/// looking at this page (identity is resolved server-side at every uplink, ADR-0018 — a booth is
/// never told), and what the navigation offers (the islands the frame declares). A frame that
/// wanted these to be static would have to know the user and the deployment.
pub const USER_SLOT: &str = "user";
/// The navigation's items — the same rows the pager switches between, so a click cannot name a
/// page that is not there.
pub const NAV_SLOT: &str = "nav";

/// The module a region shows until its booth has drawn: a spinner, because an empty region and a
/// broken one look the same and neither says which it is. Served from the renderer bundle (fluxen's
/// ADR-0007 module contract: mount/update/resize/unmount, the module owns its own loop).
pub const LOADING_MODULE: &str = "/assets/loading.js";
/// The pager: one row per island, each row a case bound to that island's region. Switching is LOCAL
/// (the pager and the navigation read the same `page` value), so it costs no round trip — nothing
/// about a click needs to reach the venue.
pub const PAGES_SLOT: &str = "pages";
pub const PAGE_SLOT: &str = "page";

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

/// Who an emit is for. The wire names ONE address — a `device` (one connection's name) or a
/// `user` (an account, reached through every device live for it) — and names none for everyone
/// (ADR-0018 §3: a booth cannot enumerate devices, so cross-device delivery is necessarily a
/// fan-out the venue performs).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Reach {
    Device(identity::DeviceId),
    User(identity::UserId),
}

/// Read the address the emitter stated. A malformed one is an ERROR, not an absent one:
/// `device: 12` used to fall through `.and_then(as_str)` into the broadcast arm, which is the
/// one failure mode where a typo reaches an unrelated screen. Addresses are TEXT because a
/// 128-bit id does not fit a JSON number (a browser's numbers are f64).
fn reach_of(payload: &Value) -> Result<Option<Reach>, String> {
    if let Some(v) = payload.get("device").filter(|v| !v.is_null()) {
        let text = v
            .as_str()
            .ok_or_else(|| format!("`device` is not text: {v}"))?;
        let id = text
            .parse::<identity::DeviceId>()
            .map_err(|e| format!("`device` `{text}`: {e}"))?;
        return Ok(Some(Reach::Device(id)));
    }
    if let Some(v) = payload.get("user").filter(|v| !v.is_null()) {
        let text = v.as_str().ok_or_else(|| format!("`user` is not text: {v}"))?;
        let id = text
            .parse::<identity::UserId>()
            .map_err(|e| format!("`user` `{text}`: {e}"))?;
        // Anonymous is a state, not an account: addressing it would reach every signed-out
        // screen in the venue, which is what a broadcast already means — so say broadcast.
        if identity::is_anonymous(id) {
            return Err(format!(
                "`user` `{text}` is the anonymous sentinel, not an account"
            ));
        }
        return Ok(Some(Reach::User(id)));
    }
    Ok(None)
}

/// Does a live connection receive this emit? ONE predicate for both legs, so the renderer's
/// fan-out and the peer leg cannot disagree about what an address means. An anonymous
/// connection is bound to no account, so no `user` reaches it: the idle window and a logout
/// both keep the device's name and withhold the permission, and this is where that shows.
pub(crate) fn reached(
    reach: Option<&Reach>,
    device: identity::DeviceId,
    user: identity::UserId,
) -> bool {
    match reach {
        None => true,
        Some(Reach::Device(d)) => *d == device,
        Some(Reach::User(u)) => !identity::is_anonymous(user) && user == *u,
    }
}

/// Where a booth's DATA payload goes on the programmatic leg: the same event, `{ev, args}`,
/// to the peer connections it addresses. A renderer draws an operation list; a program reads
/// the data shape — one emit, two legs (`gravity ADR-0009`'s two renderings).
pub(crate) type PeerDeliver = Arc<dyn Fn(Option<&Reach>, Option<&str>, &str, Value) + Send + Sync>;

/// Where a frame goes when its account has no screen awake: the venue's held queue for that
/// account (`Gateway::flush_missed` hands it back when one signs in).
/// Where a frame goes when its account has no screen awake. The BOOTH travels with it: a frame is
/// owed to the person, but only the booth that owns the region can draw it, and its own screen is
/// what says where things stand (`ctx.origin`, stamped by the venue at emit time).
pub(crate) type Defer =
    Arc<dyn Fn(&str, identity::UserId, &str, Vec<content::Content<Value>>) + Send + Sync>;

/// Install the venue's one subscription: what the application emits as a VIEW event is
/// pushed to the renderers (and, as data, to a program). A Rust body because the venue is IN this process — the frames go
/// to sockets it owns, not over a carrier, and no script has to learn about sockets.
pub(crate) async fn install(
    engine: &aura_engine::Engine,
    conns: UiConns,
    peers: PeerDeliver,
    defer: Defer,
    islands: Vec<String>,
) {
    // The islands this deployment's frame declares. Passed as a value, not read per event: the
    // frame is fixed at boot, so recomputing it per delivery would only give the same answer
    // slower — and a frame that changed mid-life would silently move every subsequent frame.
    let handler_islands = std::sync::Arc::new(islands);
    let handler_conns = Arc::clone(&conns);
    let handler_peers = Arc::clone(&peers);
    let handler_defer = Arc::clone(&defer);
    let booth = aura_booth::BoothType {
        name: RENDERER_BOOTH.to_string(),
        body: aura_booth::Body::Rust(Arc::new(move |ctx, args| {
            let conns = Arc::clone(&handler_conns);
            let peers = Arc::clone(&handler_peers);
            let defer = Arc::clone(&handler_defer);
            let islands = Arc::clone(&handler_islands);
            // WHO SPOKE. The venue stamps the emitting booth on the queued row (aura's
            // `ctx.origin`), so a frame is attributed here without any application writing its own
            // name into its payloads — and the held queue can be keyed by booth.
            let origin = ctx.origin.clone();
            Box::pin(async move {
                forward(&conns, &peers, &defer, &islands, origin.as_deref(), args);
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
            // The venue's own fan-out is public: the gate it enforces is on the events it
            // delivers, never on the act of delivering them.
            auth: false,
        }],
        concurrency: aura_booth::Concurrency::Queued,
    };
    match engine.register(booth).await {
        Ok(()) => println!("prism: `{UI_PREFIX}<name>` reaches renderers (booth `{RENDERER_BOOTH}`, prefix route)"),
        Err(e) => eprintln!("prism: the renderer booth did not register: {e} — the UI will not update"),
    }
}

/// Put an application's frames on the renderers it addresses. Payload:
/// `{name, device? | user?, sender?, data}`; an array in `data` is DRAWN and anything else is
/// injected into the slot named by the event. The frames are the renderer's vocabulary — the
/// venue does not interpret them, which is why it can forward a layout, an append and a patch
/// alike.
/// The frame's data: who is looking, and what the navigation is.
///
/// Said ONCE per connection, before any application speaks — a page that had to wait for an
/// application to learn its own navigation would show an empty bar for as long as that application
/// took to boot, and nothing at all if it failed to.
fn frame_data(gw: &Gateway, device: identity::DeviceId) -> Vec<Content<Value>> {
    let mut ops: Vec<Value> = Vec::new();
    if let Some(shell) = &gw.deployment.shell {
        // WHO. A device's alias is the user's own word for this screen; empty means "show the id",
        // and an unbound device is a guest — not an error, and not nothing to render: the frame's
        // header says who is looking even when the answer is "nobody yet".
        let (shown, bound) = match gw.registry.device(device) {
            Ok(Some(d)) if !d.alias.trim().is_empty() => (d.alias.clone(), !identity::is_anonymous(d.user)),
            Ok(Some(d)) => (short_id(&device.to_string()), !identity::is_anonymous(d.user)),
            _ => (short_id(&device.to_string()), false),
        };
        let name = if bound { shown } else { format!("{shown} (guest)") };
        ops.push(json!({
            "action": "set",
            "slot": USER_SLOT,
            "data": {
                "type": "text",
                "attrs": {"class": ["--user"]},
                "bind": {"value": {"kind": "default", "default": name}},
            },
        }));
        // WHAT THE NAVIGATION OFFERS: one item per island the frame declares, labelled by the frame
        // itself. Ids are the booth names, so a click can name its destination without a table.
        for region in shell.regions() {
            ops.push(json!({
                "action": "append",
                "slot": NAV_SLOT,
                "data": {
                    "id": region.booth,
                    "type": "text",
                    "attrs": {"class": ["--nav-item"]},
                    "bind": {"value": {"kind": "default", "default": region.label}},
                },
            }));
            // The page a navigation item switches to: the SAME id, holding the region's binding.
            // The frame declares a region; the venue supplies the row that shows it — so a
            // deployment writes neither the switch nor the mapping, and cannot get them out of step.
            ops.push(json!({
                "action": "append",
                "slot": PAGES_SLOT,
                "data": {
                    "id": region.booth,
                    // A `frame` bound to the island's slot: when the booth has drawn, the slot
                    // holds a tree and this renders it; until then — and if the booth never
                    // answers — its `children` are what shows. This row declares none, so the
                    // region is blank until its booth draws. (fluxen ADR-0027 merged the old
                    // `case`/`placeholder` pair into `frame`: one container, binding optional.)
                    "type": "frame",
                    "attrs": {"class": ["maximize", "col"]},
                    "bind": {"value": {"kind": "source", "source": format!("{}{}", crate::shell::ISLAND, region.booth)}},
                    // UNTIL THE BOOTH DRAWS. A `frame` renders its `children` whenever the slot it
                    // mirrors is empty (fluxen ADR-0027), so this is the region's default content —
                    // one canvas, no wrapper: the spinner centres itself.
                    "children": [{
                        "type": "canvas",
                        "url": LOADING_MODULE,
                        "attrs": {"class": ["maximize"]},
                        // A canvas carries a PAYLOAD or it does not exist: an empty one is
                        // collapsed and never mounted (fluxen ADR-0012), so a spinner with no
                        // data would be an invisible node. The size rides the payload — the
                        // module's only knob, and the same fact for every region.
                        "bind": {"value": {"kind": "default", "default": {"size": "32px"}}},
                    }],
                },
            }));
        }
    }
    ops_of(ops, "shell")
}

/// The first eight characters of an id: enough to tell two devices apart, short enough to read.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// One island's frame, with its slots moved into the island's namespace.
///
/// The EMPTY slot is left alone: it is `create`'s address for the layout root (fluxen's
/// convention), and an application that creates a root inside a frame is doing the one thing the
/// shell exists to prevent — the venue does not help it by renaming the target.
///
/// An explicit `island:` prefix is left alone too. That is the escape hatch, and it is deliberately
/// not symmetrical with the default: a booth may write into a region whose name it was TOLD, and
/// that name is the only thing standing between two applications' slots.
fn namespace(ops: Vec<Content<Value>>, island: &str) -> Vec<Content<Value>> {
    fn one(slot: &str, island: &str) -> String {
        if slot.is_empty() || slot.starts_with(crate::shell::ISLAND) {
            slot.to_string()
        } else {
            format!("{}{island}/{slot}", crate::shell::ISLAND)
        }
    }
    ops.into_iter()
        .map(|op| match op {
            // A `set` with the EMPTY slot is "my surface": the booth has nothing to call it, and
            // the venue is the only party that knows what the page calls it. Every other operation
            // treats empty as "no address" and leaves it alone — `create`'s empty slot is the page
            // root, and renaming it would help an application replace the frame.
            Content::Set(mut x) => {
                x.slot = if x.slot.is_empty() {
                    format!("{}{island}", crate::shell::ISLAND)
                } else {
                    one(&x.slot, island)
                };
                Content::Set(x)
            }
            Content::Append(mut x) => {
                x.slot = one(&x.slot, island);
                Content::Append(x)
            }
            Content::Remove(mut x) => {
                x.slot = one(&x.slot, island);
                Content::Remove(x)
            }
            Content::Patch(mut x) => {
                x.slot = one(&x.slot, island);
                Content::Patch(x)
            }
            Content::Inject(mut x) => {
                x.slot = one(&x.slot, island);
                Content::Inject(x)
            }
            other => other,
        })
        .collect()
}

/// Which slot of a frame a CONTENT payload rides in, and the key that marks one.
///
/// gravity has two content modes and two encodings (gravity ADR-0013); encoding is a transport
/// axis, so the frame does not say which codec it used — the SHAPE says. A plain frame's `data`
/// is the envelope itself (a map with `name`/`island`/`data`/…); encoded content arrives as a map
/// whose ONLY key is [`CBOR_PAYLOAD_KEY`], holding the CBOR document as base64.
///
/// Base64 rather than a CBOR byte string because the payload is a `serde_json::Value` at every
/// hop between the booth and here (the carrier's frame type, the realm's event args), and a
/// `Value` has no bytes variant: a raw byte string is a decode error there, so a frame carrying
/// one would never arrive at all. The marker keeps the discriminator on the wire and in the
/// frame's own shape, so nothing has to be configured out of band.
const CBOR_PAYLOAD_KEY: &str = "$cbor";

/// The bytes a `data` carries when it is ENCODED content, `None` when it is the frame itself.
fn cbor_bytes(data: &Value) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let object = data.as_object()?;
    if object.len() != 1 {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(object.get(CBOR_PAYLOAD_KEY)?.as_str()?)
        .ok()
}

/// THE ONE DECODE SITE for the codec axis: a frame whose `data` is CBOR is turned back into the
/// same `Value` the JSON path sends, and every other frame is returned untouched.
///
/// Called once, at the top of [`forward`] — the funnel every booth frame arrives through — so no
/// second copy of this rule can drift, and no reader below has to know a codec exists.
fn decode_content(mut payload: Value) -> Value {
    let Some(bytes) = payload.get("data").and_then(cbor_bytes) else {
        return payload;
    };
    match ciborium::from_reader::<Value, _>(bytes.as_slice()) {
        Ok(decoded) => {
            if let Some(object) = payload.as_object_mut() {
                object.insert("data".to_string(), decoded);
            }
            payload
        }
        // An undecodable payload is STATED, and the frame is forwarded as it came: dropping a
        // frame on a decode error would make a codec bug look like a booth that said nothing.
        Err(e) => {
            let name = payload.get("name").and_then(Value::as_str).unwrap_or("?");
            eprintln!("prism: view `{name}` carries an undecodable CBOR payload: {e}");
            payload
        }
    }
}

pub(crate) fn forward(
    conns: &UiConns,
    peers: &PeerDeliver,
    defer: &Defer,
    islands: &[String],
    origin: Option<&str>,
    payload: Value,
) {
    // THE CODEC AXIS, applied ONCE (gravity ADR-0013): a frame whose `data` is a CBOR payload
    // becomes the same `Value` the JSON path sends, and everything below reads `payload` — so
    // the route, the address, the drawing and the peer leg are all codec-blind.
    let payload = decode_content(payload);
    // THE ENVELOPE CARRIES THE ROUTE. A Rust body is not told which event fired (the concrete
    // name is consumed by the dispatcher), so the emitter states it: `name` is the recipient
    // event, marked with the same `ui:` prefix as the emit itself. The venue strips the
    // marker — the page is not a party to this vocabulary — and forwards the payload.
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        eprintln!("prism: view event without a `name`: nothing to deliver — {payload}");
        return;
    };
    let name = name.strip_prefix(UI_PREFIX).unwrap_or(name).to_string();
    // Who it is for: one device, or every live device of one account; no address is everyone.
    // `sender` is excluded either way — a page does not hear its own input echoed back.
    let reach = match reach_of(&payload) {
        Ok(reach) => reach,
        Err(e) => {
            eprintln!(
                "prism: view event `{name}` carries an unusable address, nothing delivered: {e}"
            );
            return;
        }
    };
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
            for k in ["name", "device", "user", "sender"] {
                o.remove(k);
            }
        }
        rest
    });
    // A PROGRAM IS A CLIENT TOO: the data shape (everything that is not an operation list)
    // reaches the peer connections the event addresses, as `{ev, args}` — the same event the
    // renderer gets, minus the drawing.
    if !body.is_array() {
        peers(reach.as_ref(), sender.as_deref(), &name, body.clone());
    }
    let ops: Vec<Content<Value>> = match body {
        Value::Array(frames) => ops_of(frames, "view"),
        other => inject(&name, &name, other),
    };
    // A NAMESPACE THE APPLICATIONS CANNOT SPELL (ADR-0019). An island booth addresses its own
    // slots by their bare names (`pages`, `notice`) and the venue puts the prefix on — so writing
    // into ANOTHER island's region takes knowing its name on the page, which is a fact no booth is
    // told and no booth can enumerate. The name is the event's first segment (`ui:gravity-model.…`
    // ⇒ `gravity-model`), NOT the aura booth type: a page's name for a surface is the page's
    // business, and the engine is not something the renderer knows about.
    // The booth STATES its region (`island` in the envelope). Deriving it from the event name was
    // the first attempt and it was wrong: an event says what HAPPENED (`ui:layout`,
    // `ui:turn.done`), and most names carry no booth segment at all — the ones that do
    // (`ui:gravity-model.screen`) are the exception, not the rule.
    //
    // A name the frame does not declare gets no namespace and a warning: either the deployment is
    // inconsistent (two spellings of one region) or the booth is writing outside its island, and
    // both are worth seeing rather than silently drawing into a slot nobody binds.
    let ops = match payload.get("island").and_then(Value::as_str) {
        Some(whose) if islands.iter().any(|i| i == whose) => namespace(ops, whose),
        Some(unknown) => {
            eprintln!(
                "prism: view `{name}` claims island `{unknown}`, which the frame does not declare \
                 ({islands:?}) — its slots keep their bare names"
            );
            ops
        }
        None => ops,
    };
    if ops.is_empty() {
        eprintln!("prism: view event `{name}` carried nothing deliverable");
        return;
    }
    let conns = conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut sent = 0usize;
    let mut awake = false;
    for c in conns.values() {
        let here = c.device.to_string();
        let user = *c
            .user
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if reached(reach.as_ref(), c.device, user) {
            awake = true;
        }
        if sender.as_ref() == Some(&here) || !reached(reach.as_ref(), c.device, user) {
            continue;
        }
        let _ = c.tx.send(ops.clone());
        sent += 1;
    }

    if sent == 0 {
        // AN ACCOUNT WITH NO SCREEN AWAKE. The frame is not lost: the venue holds it for that
        // account and hands it over when one of their devices proves itself (`flush_missed`).
        // A DEVICE address has no such owner — that screen is simply gone — so it is only a log.
        match reach.as_ref() {
            Some(Reach::User(u)) if !awake && !identity::is_anonymous(*u) => {
                eprintln!("prism: view event `{name}` held for {u}: no screen of that account is awake");
                defer(origin.unwrap_or_default(), *u, &name, ops);
            }
            _ => eprintln!(
                "prism: view event `{name}` reached no renderer (address {reach:?}, sender {sender:?})"
            ),
        }
    }
}

/// Serve one renderer connection to completion.
pub(crate) async fn serve(
    gw: Arc<Gateway>,
    ws: WebSocketStream<Prefixed>,
    codec: ActiveCodec,
    identified: identity::Identified,
) -> anyhow::Result<()> {
    let identity::Identified { device, user: user0, token, idle } = identified;
    let (mut write, mut read) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<Content<Value>>>();
    let (id, user) = gw.register_ui(tx, device, user0);
    // A device whose token still names the account is that account's screen from the first frame;
    // whatever arrived while no screen of theirs was awake is owed to it now.
    if !identity::is_anonymous(user0) {
        gw.flush_missed(device, user0).await;
    }

    // The connection starts the clock. A device that was silent past the window is told so on the
    // wire (a notice with a reason, not silence): its token still NAMED it — that is how its alias
    // survives — but the name carried no permission until a login re-earned it, and the page has to
    // know that or it will keep presenting a credential that buys nothing.
    if let Err(e) = gw.registry().touch(device) {
        eprintln!("prism: /channel could not stamp activity: {e}");
    }
    if idle {
        say(&gw, device, NOTICE_EVENT, json!({
            "detail": "this device was silent past the idle window; log in again",
            "reason": "idle",
        }))
        .await;
    }
    let mut spoke_at = identity::now_ms();
    let mut persisted_at = spoke_at;

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
                Err(e) => eprintln!("prism: /channel encode failed: {e}"),
            }
        }
        // Channel closed (the table dropped our sender — revocation or disconnect): close
        // the socket properly so the page SEES it instead of hanging on a dead write half.
        let _ = write.send(Ws::Close(None)).await;
    });

    // The connect answer: the CREDENTIAL, in the renderer's shape. This is the only fact carrying
    // it — the page keeps the token and presents it on its next connect, which is how the same
    // device (and its alias) comes back. Who it is, nobody needs to say: the venue resolves the
    // device to a user at every uplink, so the user is a server-side answer, not a page-side one.
    say(&gw, device, TOKEN_EVENT, token_fact(device, token)).await;
    eprintln!("prism: renderer leg: device {device} connected (conn {id})");

    // THE CONNECTION IS A FACT, AND THE LAYOUT IS THE APPLICATION'S ANSWER TO IT. The venue says
    // `ui:connect {device}` on the plane; whichever booth declared it answers with frames, and
    // the page draws whatever arrives. Nothing is configured here: a deployment has no layout
    // source to name, and a venue with no such booth draws nothing while the connection stays up.
    // Said through `say` like the identity fact, so the envelope names the recipient event: a bare
    // payload is dropped by `forward` (this venue's one speech path).
    // THE FRAME COMES FIRST (ADR-0019). A page lives in a shell the operator wrote: navigation, and
    // one empty region per island. The regions exist BEFORE any boat is asked to fill them, because
    // a booth that answers `ui:connect` with a `set` needs the slot to be there — and a booth cannot
    // draw the frame itself, since it holds regions belonging to booths it cannot see.
    if let Some(shell) = &gw.deployment.shell {
        // The frame, then its data. One message would work — the renderer applies a list in order —
        // but two keeps the FRAME (a deployment fact, from a file) apart from the FACTS ABOUT THIS
        // VIEWER (who is looking, what they may open), which is the same split the rest of the
        // venue follows.
        gw.ui_frames(device, ops_of(vec![shell.create()], "shell"));
        gw.ui_frames(device, frame_data(&gw, device));
    }
    let island = gw.active_island(user0);
    gw.connect_event(device, island).await;

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
        // THE WINDOW IS CHECKED PER MESSAGE, not per connection: a socket can live for days, and a
        // connection nobody has used for longer than the window is not authenticated any more. The
        // page stays connected — it is told to log in, which is cheaper than a reconnect and is the
        // reason `disconnect` needs no bookkeeping. The auth verbs are NOT gated: refusing the very
        // message that re-earns the trust would leave the page with no way back.
        let verb = matches!(uplink.dest.as_str(), "login" | "signup" | "logout");
        let now = identity::now_ms();
        if !verb && now.saturating_sub(spoke_at) > gw.registry().idle_ms() {
            crate::auth_set(&user, identity::anonymous());
            said_too_late(&gw, device).await;
            continue;
        }
        // The clock is persisted on a THROTTLE: the live decision reads `spoke_at` in memory, and
        // the row only has to be accurate enough for the NEXT connect — so a busy connection does
        // not write per message. The cap is an hour: a tenth of a 30-day window would let the stored
        // value lag three days behind reality, and the next connect would then judge a device idle
        // that had been talking the whole time.
        spoke_at = now;
        let throttle = (gw.registry().idle_ms() / 10).min(60 * 60 * 1000);
        if now.saturating_sub(persisted_at) >= throttle {
            persisted_at = now;
            if let Err(e) = gw.registry().touch(device) {
                eprintln!("prism: /channel could not stamp activity: {e}");
            }
        }
        dispatch(&gw, &user, device, token, uplink).await;
    }

    // THE LAST FACT OF A CONNECTION: said BEFORE the table forgets the device, so a booth holding
    // a conversation for it hears that it is over. The page itself is gone (this is the read half
    // ending), which is why the ruling calls this a fact for applications and not a notice.
    // One last stamp on the way out: the throttle above can leave the row up to an hour behind, and
    // a connection's end is exactly when the insight "this device is idle from here" becomes true.
    if let Err(e) = gw.registry().touch(device) {
        eprintln!("prism: /channel could not stamp the last activity: {e}");
    }
    say(&gw, device, DISCONNECT_EVENT, json!({ "device": device.to_string() })).await;
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
    // A PAGE'S `dest` IS THE BARE NAME (the venue adds `ui:` on the way in and strips it on the
    // way out). An application author who copies the plane's name sends `ui:turn.submit`, and the
    // venue reads the marker as "this is already a view event" and paints it — it reaches no
    // renderer and the turn never starts, which reads exactly like a broken venue. Say where the
    // mistake is, once, at the door.
    if let Some(bare) = asked.strip_prefix("ui:") {
        eprintln!(
            "prism: /channel uplink dest `{asked}` carries the plane's marker — a page addresses \
             `{bare}` (the venue adds the marker); this uplink is not routed"
        );
    }
    match asked.as_str() {
        "login" | "signup" => {
            // ONE SEAM FOR BOTH LEGS: the booth proves the credential (a call — a password never
            // rides the event plane), the venue binds the device and rotates the token.
            let op = if asked == "signup" { crate::auth::OP_REGISTER } else { crate::auth::OP_VERIFY };
            match gw.login_op(op, device, &data).await {
                Ok((user_id, fresh)) => {
                    crate::auth_set(user, user_id);
                    // Proving the account makes this screen its owner: whatever arrived while no
                    // screen of theirs was awake is owed here, before anything else is said.
                    gw.flush_missed(device, user_id).await;
                    // Two speeches, both facts: `token` carries what only it can (the ROTATED
                    // credential the page must keep — the old one is dead the moment this is
                    // issued), and `login` is the lifecycle event an application binds to.
                    say(&gw, device, TOKEN_EVENT, token_fact(device, fresh)).await;
                    say(&gw, device, LOGIN_EVENT, json!({
                        "device": device.to_string(),
                        "user": user_id.to_string(),
                    }))
                    .await;
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
                say(&gw, device, TOKEN_EVENT, token_fact(device, fresh)).await;
                say(&gw, device, LOGOUT_EVENT, json!({ "device": device.to_string() })).await;
            }
            Err(e) => {
                eprintln!("prism: /channel logout refused: {e}");
                say(&gw, device, NOTICE_EVENT, json!({"detail": e.to_string()})).await;
            }
        },
        // The account's own devices, on this leg too: the venue answers with a FACT (the page
        // binds slot `devices`), because a booth cannot enumerate devices either. Anonymous gets
        // the same refusal a program gets, as a notice the person can read.
        // WHERE THE PERSON IS. The frame names the islands; this says which one they walked to. It
        // is remembered on the ACCOUNT (they are one person across machines) and answered with the
        // same `connect` a fresh page gets — so the booth that owns that island draws it, lazily,
        // and the ones the person is not looking at say nothing at all.
        "island" => {
            // The payload is the value this click chose: a renderer reports the bare value
            // (fluxen ADR 0028 — a report states what happened, it is not something a slot
            // subscribes to), while a program may still send `{booth: ...}`. Both are the same
            // fact; the deployment names it.
            let chosen = crate::text_field(&json!(data), "booth")
                .or_else(|| data.as_str().map(str::to_string));
            let known = chosen
                .as_ref()
                .filter(|b| gw.deployment.shell.as_ref().is_some_and(|s| s.islands().contains(b)));
            match known {
                Some(booth) => {
                    let who = crate::auth_get(user);
                    if !identity::is_anonymous(who) {
                        if let Err(e) = gw.registry().remember_island(who, booth) {
                            eprintln!("prism: could not remember island `{booth}`: {e}");
                        }
                    }
                    gw.connect_event(device, Some(booth.clone())).await;
                }
                None => {
                    say(
                        &gw,
                        device,
                        NOTICE_EVENT,
                        json!({"detail": format!("`{}` is not an island of this deployment", chosen.unwrap_or_default())}),
                    )
                    .await
                }
            }
        }
        "devices" => match gw.device_rows(device, crate::auth_get(user)) {
            Ok(rows) => {
                say(
                    &gw,
                    device,
                    DEVICES_EVENT,
                    json!({ "device": device.to_string(), "devices": rows }),
                )
                .await
            }
            Err(e) => say(&gw, device, NOTICE_EVENT, json!({ "detail": e.to_string() })).await,
        },
        // `alias` names THIS device: the connection's own row, so it needs no address and no
        // permission — a name is not a decision.
        "alias" => {
            let named = crate::clean_alias(data.get("alias").and_then(Value::as_str).unwrap_or_default())
                .and_then(|alias| gw.registry().set_alias(device, &alias).map(|()| alias));
            match named {
                Ok(alias) => {
                    say(
                        &gw,
                        device,
                        ALIAS_EVENT,
                        json!({ "device": device.to_string(), "alias": alias }),
                    )
                    .await
                }
                Err(e) => say(&gw, device, NOTICE_EVENT, json!({ "detail": e.to_string() })).await,
            }
        }
        other => {
            // THE DECLARATION STILL GATES AT THE VENUE. An anonymous connection never gets to
            // put the event on the plane at all, so a booth that declared `auth` cannot be
            // reached by someone it did not agree to serve — the check stays on this side of
            // delivery, not inside every booth.
            let bound = crate::auth_get(user);
            if identity::is_anonymous(bound) && gw.requires_auth(&format!("{UI_PREFIX}{other}")).await {
                eprintln!("prism: /channel event `{other}` refused: authentication required");
                // THE VENUE KNOWS IT REFUSED, so it says so as a FACT: an application never learns
                // this (nothing was delivered to it), and a surface that only opens on a click is a
                // surface nobody finds at the moment it is needed. The auth booth subscribes; the
                // notice below is for the person, this is for the platform.
                say(&gw, device, crate::auth::AUTH_REQUIRED_FACT, json!({"event": other})).await;
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
            // A STOP IS NOT AN EVENT (ADR-0008 as revised): the venue holds authority over
            // this connection, so a cancel is served HERE by setting the realm's stop bit on
            // the sender's own instance — the running turn reads it at its next chunk
            // boundary. Emitting it would address a booth that no longer listens: a delivery
            // to the instance that is running the turn waits behind it, which is why the
            // event shape was retired.
            if event == gravity_booth::CANCEL_EVENT {
                let target = gravity_booth::turn_instance(&device.to_string());
                let realm = gw.engine.realm.lock().await;
                // A CLICK MEANS "NOW", so it is both actions (aura ADR-0047 §2): the bit
                // stops the turn at its next boundary, the interrupt stops the wait — which
                // is the one that reaches a turn parked inside a single provider pull. The
                // bit is not redundant: a turn that finishes cleanly between chunks should
                // still record that it was asked to stop, and only the bit is readable from
                // inside the guest.
                realm.stop_set(&target);
                realm.stop_interrupt(&target);
                return;
            }
            let envelope = json!({
                "name": event,
                // WHETHER A FRAME WAS ALREADY DRAWN (ADR-0019). Every delivery says it, not just the
                // connect: a booth that redraws the whole page inside a shell erases the frame and
                // every other island in it, and it cannot discover that from the page — the page is
                // not a party it talks to.
                "shell": gw.deployment.shell.is_some(),
                // The ADDRESS is a device (ADR-0018: a channel is an application's business) and
                // the payload is `data` — the shape a booth reads as a delivery, not a request.
                //
                // `id` is the uplink's own id, and it must travel: a form's id IS the address of
                // the step it answers (the run, the step, the pass, the attempt). Dropping it here
                // left every answer with nothing to answer — the venue saw the event, the booth
                // saw a payload, and the run waited for a reply nobody could address.
                "device": device.to_string(),
                "id": uplink.id.clone(),
                // ONE SENDER SHAPE, from the one place that can say it: the venue's registry is
                // what knows an account's name, so the same helper builds the sender on every leg
                // (a renderer's uplink is a sender like any other).
                "sender": gw.sender_json(device, bound),
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
    // A NOTICE IS A ROW. `notice` is a list slot the page lays out as a rack, so a fresh notice is
    // APPENDED with its own id instead of replacing the last one — and no row is ever taken back:
    // the floating window is the one that lets each row expire (its rack's business), while a plain
    // rack on the same slot still shows every notice the device has seen. Everything else the venue
    // says is a fact about now, and takes the value slot named by the fact.
    let data = if fact == NOTICE_EVENT {
        json!([{ "action": "append", "slot": NOTICE_EVENT, "data": notice_row(&data) }])
    } else {
        data
    };
    let envelope = json!({ "name": event, "device": device.to_string(), "data": data });
    // `emit` already logs a delivery failure (Gateway::emit); nothing to add here.
    gw.emit(&event, envelope).await;
}

/// A notice row: an id (row identity lives in the data, the rack's rule) and the words. A text node
/// is the least the venue can say — the frame's row template decides how it looks.
fn notice_row(data: &Value) -> Value {
    let text = data
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("something went wrong")
        .to_string();
    // A text node carries no words of its own: what it says comes from its binding (fluxen's
    // `text` reads `bind["value"]`), which is also why a row can be a text node without the venue
    // inventing a layout.
    let mut row = json!({
        "id": okm_core::Uuid7::now().to_string(),
        "type": "text",
        "bind": { "value": { "kind": "default", "default": text } },
    });
    // What a PERSON reads is the text, but the venue attaches facts a PAGE may act on — the idle
    // refusal carries `reason: "idle"` — and dropping them at this seam would make every such
    // signal prose that only a human can read. They ride beside the row: the renderer draws the
    // text node and ignores what it does not bind, while a page reads the field it wants.
    if let Some(fields) = data.as_object() {
        for (key, value) in fields {
            if key != "detail" {
                row[key] = value.clone();
            }
        }
    }
    row
}

/// The identity fact: who this connection is, and the credential that keeps saying so. Ids travel
/// as text; an anonymous connection says so with `null` rather than a nil uuid.
fn token_fact(device: identity::DeviceId, token: identity::TokenId) -> Value {
    json!({ "device": device.to_string(), "token": token.to_string() })
}

/// The same notice the connect path says, said when a LIVE connection crosses the window: the socket
/// stays, the uplinks stop being accepted, and a login is what starts them again.
async fn said_too_late(gw: &Gateway, device: identity::DeviceId) {
    eprintln!("prism: /channel device {device} crossed the idle window; asking for a login");
    say(gw, device, NOTICE_EVENT, json!({
        "detail": "this device was silent past the idle window; log in again",
        "reason": "idle",
    }))
    .await;
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use serde_json::json;

    fn slots(ops: Vec<Content<Value>>) -> Vec<String> {
        ops.into_iter()
            .map(|op| match op {
                Content::Create(x) => x.slot,
                Content::Set(x) => x.slot,
                Content::Append(x) => x.slot,
                Content::Remove(x) => x.slot,
                Content::Patch(x) => x.slot,
                Content::Inject(x) => x.slot,
                other => panic!("unexpected op: {other:?}"),
            })
            .collect()
    }

    /// The default: an application writes its bare names and the venue puts them in its namespace.
    /// Five operations, because a namespace that only covered `set` would let `append` write into
    /// another island's list — the same hole with a different door.
    #[test]
    fn every_operation_is_namespaced_not_just_set() {
        let ops: Vec<Content<Value>> = serde_json::from_value(json!([
            {"action": "set", "slot": "pages", "data": {}},
            {"action": "append", "slot": "notice", "position": 0, "data": {}},
            {"action": "remove", "slot": "notice", "id": "1"},
            {"action": "patch", "slot": "scene", "path": "/nodes", "op": "replace", "value": 1},
            {"action": "inject", "slot": "messages", "data": {}},
        ]))
        .unwrap();
        assert_eq!(
            slots(namespace(ops, "gravity-model")),
            vec![
                "island:gravity-model/pages",
                "island:gravity-model/notice",
                "island:gravity-model/notice",
                "island:gravity-model/scene",
                "island:gravity-model/messages",
            ]
        );
    }

    /// Two exceptions and one rule of their own. A `set` into the EMPTY slot is "my surface" — the
    /// booth has no name for it, so the venue supplies one. `create`'s empty slot is the page ROOT
    /// and is left alone, because renaming it would help an application replace the frame. An
    /// explicit `island:` prefix is the escape hatch for a region whose name this booth was told.
    #[test]
    fn my_surface_gets_a_name_and_the_root_does_not() {
        let ops: Vec<Content<Value>> = serde_json::from_value(json!([
            {"action": "create", "slot": "", "data": {}},
            {"action": "set", "slot": "", "data": {}},
            {"action": "set", "slot": "island:admin/notice", "data": {}},
        ]))
        .unwrap();
        assert_eq!(
            slots(namespace(ops, "gravity-model")),
            vec!["", "island:gravity-model", "island:admin/notice"],
            "the root stays the root; the empty slot of a `set` is this island's surface"
        );
    }

    // ---------------------------------------------- the content codec axis (gravity ADR-0013) --

    /// The frame a booth emits, in the two shapes the axis produces: `data` is the envelope
    /// (JSON mode) or `{"$cbor": <base64 of the same value>}` (CBOR mode).
    fn both_shapes(content: &Value) -> (Value, Value) {
        use base64::Engine as _;
        let mut document = Vec::new();
        ciborium::into_writer(content, &mut document).expect("cbor document");
        let plain = json!({
            "name": "ui:layout",
            "island": "gravity-model",
            "data": content,
        });
        let encoded = json!({
            "name": "ui:layout",
            "island": "gravity-model",
            "data": { "$cbor": base64::engine::general_purpose::STANDARD.encode(&document) },
        });
        (plain, encoded)
    }

    /// THE EQUIVALENCE, proved at the funnel: one frame, encoded both ways, decoded both ways,
    /// and the two decoded frames EQUAL — through the same drawing step the venue uses, so the
    /// assertion covers the ops a renderer would get, not just the value's shape.
    #[test]
    fn a_cbor_payload_and_a_plain_map_reach_the_renderer_as_the_same_frame() {
        let content = json!([
            { "action": "append", "slot": "messages", "data": {
                "id": "7_1", "type": "frame", "attrs": { "selector": "assistant" },
                "children": [{ "type": "text", "source": { "default": "there" } }],
            }},
            { "action": "patch", "slot": "messages", "id": "7_1",
              "path": "/children/0/source/default", "op": "append", "value": "!" },
        ]);
        let (plain, encoded) = both_shapes(&content);

        let decoded_plain = decode_content(plain.clone());
        let decoded_cbor = decode_content(encoded.clone());
        assert_eq!(
            decoded_plain, decoded_cbor,
            "the two encodings are one frame after the funnel"
        );
        assert_eq!(decoded_plain, plain, "and the JSON path is untouched");
        assert_eq!(
            decoded_cbor["data"], content,
            "the byte path is the content the JSON path sent"
        );

        // The drawing step agrees, not just the value: same slots, same order.
        let draw = |frame: &Value| {
            let frames = frame["data"].as_array().cloned().unwrap_or_default();
            slots(namespace(ops_of(frames, "view"), "gravity-model"))
        };
        assert_eq!(draw(&decoded_plain), draw(&decoded_cbor));
    }

    /// Today's wire must keep working byte for byte: a frame whose `data` is a map is returned
    /// exactly as it came — including a map that happens to carry a `$cbor` key BESIDE others,
    /// which is a plain payload and not an encoded one.
    #[test]
    fn the_decode_is_a_no_op_unless_the_payload_is_the_marker_and_nothing_else() {
        let plain = json!({ "name": "ui:layout", "island": "gravity-model",
                            "data": { "name": "ui:x", "data": [1, 2] } });
        assert_eq!(decode_content(plain.clone()), plain);

        let lookalike = json!({ "name": "ui:layout",
                                "data": { "$cbor": "AQ==", "type": "frame" } });
        assert_eq!(decode_content(lookalike.clone()), lookalike);

        // No `data` at all: a page's own uplink, handled below by the `args` branch.
        let uplink = json!({ "name": "ui:connect", "args": { "device": "d" } });
        assert_eq!(decode_content(uplink.clone()), uplink);

        // An unparsable payload is forwarded as it came, never dropped.
        let broken = json!({ "name": "ui:layout", "data": { "$cbor": "not base64!!" } });
        assert_eq!(decode_content(broken.clone()), broken);
    }
}
