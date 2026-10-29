//! The renderer's leg: `/ui` speaks fluxen's wire (ADR-0017 §4/§8 + its 2026-10-29 Update).
//!
//! Two legs, two shapes, and this file is the second one:
//!
//! - `/ws` — the programmatic leg (CLI, peers, tests): `{ev, args}` frames, the shape the
//!   rest of this crate dispatches.
//! - `/ui` — the browser leg: downstream `content::Message {ev, sender, created?, content}`,
//!   upstream `content::Outflow {event, id?, data}`. The renderer's own vocabulary, which is
//!   the whole point: fluxen is pure UI and never adapts to a host, so the host adapts here.
//!
//! Prism's own verbs ride this leg as `ev != "draw"` frames — the renderer ignores them by
//! design (its ADR 0003 operation channel), so one socket still carries ONE frame shape. The
//! verbs are `connected`, `init`/`init.result`/`init.error`, `login`/`signup`/`logout` with
//! their `.result` frames, and `error`. Everything else is an ordinary application event,
//! dispatched to the booth NAMED BY THE EVENT (the peer leg's rule: type == handler == `ev`),
//! and its answer is VIEW FRAMES — because a handler may return a layout, one operation, or
//! a list, and the renderer only knows how to draw.
//!
//! The instance key is `Named("ui")`: the renderer's leg is one instance for now. Per-client
//! partitioning arrives with sessions, and that is the line that will change.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use content::codec::ActiveCodec;
use content::{Content, Message, Outflow};
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

/// The renderer connection table: id → connection. The venue owns it, the forwarding booth
/// writes into it, and the peer-plane acts (revocation) reach it.
pub(crate) type UiConns = Arc<std::sync::Mutex<std::collections::HashMap<u64, UiConn>>>;

/// The booth type the venue registers for itself: the application emits, this forwards.
pub const RENDERER_BOOTH: &str = "renderer";

/// The event an application emits to put frames on a renderer. One name, declared once.
pub const VIEW_EVENT: &str = "view";

/// One renderer connection: its writer channel and the auth field a `login` writes. The
/// table lives on [`Gateway`] so the acts that must reach every leg (revocation) see it.
pub(crate) struct UiConn {
    pub tx: mpsc::UnboundedSender<Message<Value>>,
    pub device: u64,
    pub user: Arc<AtomicU64>,
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
        receives: vec![aura_booth::ReceiveDecl {
            event: VIEW_EVENT.to_string(),
            resolution: aura_booth::RouteResolution::Singleton,
            wildcard: false,
        }],
    };
    match engine.register(booth).await {
        Ok(()) => println!("prism: view events reach renderers (booth `{RENDERER_BOOTH}`, event `{VIEW_EVENT}`)"),
        Err(e) => eprintln!("prism: the renderer booth did not register: {e} — the UI will not update"),
    }
}

/// Put an application's frames on the renderers it names. Payload: `{device?: n, frames:
/// <frame or list>}`; `content` is accepted as a synonym (it is the wire's name for it), and
/// a bare frame stands alone. The frames are the renderer's vocabulary — the venue does not
/// interpret them, which is why it can forward a layout, an append and a patch alike.
pub(crate) fn forward(conns: &UiConns, payload: Value) {
    // The TARGET comes from the payload; the FRAMES come from its `frames`/`content` field
    // (or the payload is one frame). Reading the device off the frames value instead would
    // make every named event a broadcast — a fan-out bug that looks like "it works".
    let target = payload.get("device").and_then(Value::as_u64);
    let frames: Vec<Value> = match payload.get("frames").or_else(|| payload.get("content")).cloned() {
        Some(Value::Array(frames)) => frames,
        Some(one) => vec![one],
        None if payload.get("action").is_some() => vec![payload.clone()],
        None => {
            eprintln!("prism: view event carried no frames: {payload}");
            return;
        }
    };
    let conns = conns.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut sent = 0usize;
    for (id, c) in conns.iter() {
        if target.is_some_and(|t| t != c.device) {
            continue;
        }
        for frame in &frames {
            match serde_json::from_value::<Content<Value>>(frame.clone()) {
                Ok(op) => {
                    let _ = c.tx.send(draw(op));
                    sent += 1;
                }
                Err(e) => eprintln!("prism: view frame for conn {id} is not an Accrete operation: {e}"),
            }
        }
    }
    if sent == 0 {
        eprintln!("prism: view event reached no renderer (device {target:?}, {} frames)", frames.len());
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
    let (tx, mut rx) = mpsc::unbounded_channel::<Message<Value>>();
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
    gw.send_ui(id, verb("connected", &format!("device:{device}")));
    eprintln!("prism: renderer leg: device {device} connected (conn {id})");

    // THE LAYOUT IS PUSHED ON CONNECT, not asked for. A client cannot know when it is ready
    // to receive it and the venue cannot know when a page is up, so the venue states what it
    // has and the client draws whatever arrives — one ordering, no handshake, no page code.
    match gw.init_source() {
        Some(crate::init::InitSource::File(frames)) => {
            let count = frames.len();
            eprintln!("prism: renderer leg: device {device} pushed {count} layout frame(s)");
            reply_frames(&gw, id, Ok(frames.clone()), "init").await;
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
            gw.send_ui(id, verb("init.error", ""));
        }
    }

    while let Some(msg) = read.next().await {
        let bytes = match msg? {
            Ws::Binary(b) => b,
            Ws::Text(t) => t.into_bytes(),
            Ws::Close(_) => break,
            _ => continue, // ping/pong stay inside tungstenite
        };
        let uplink: Outflow = match codec.decode_auto(&bytes) {
            Ok(v) => v,
            Err(e) => {
                // A frame we cannot read is the client's bug, and its payloads are not ours
                // to guess. Say so on the log, keep the socket up.
                eprintln!("prism: /ui uplink not decodable as Outflow: {e}");
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
    let event = uplink.event.clone();
    let data = uplink.data;
    match event.as_str() {
        "login" | "signup" => {
            let username = data.get("username").and_then(Value::as_str).unwrap_or("");
            let password = data.get("password").and_then(Value::as_str).unwrap_or("");
            let nickname = data.get("nickname").and_then(Value::as_str).unwrap_or("");
            let outcome = if event == "signup" {
                gw.registry().signup(username, nickname, password, device)
            } else {
                gw.registry().login(username, password, device)
            };
            match outcome {
                Ok(user_id) => {
                    user.store(user_id, Ordering::Relaxed);
                    // Identity answers in the `sender` field — the same place `connected`
                    // carries the device, so a client reads ONE field for "who am I".
                    gw.send_ui(id, verb(&format!("{event}.result"), &format!("user:{user_id}")));
                }
                Err(e) => {
                    eprintln!("prism: /ui {event} refused: {e}");
                    gw.send_ui(id, verb("error", ""));
                }
            }
        }
        "logout" => match gw.registry().logout(device) {
            Ok(()) => {
                user.store(0, Ordering::Relaxed);
                gw.send_ui(id, verb("logout.result", &format!("device:{device}")));
            }
            Err(e) => {
                eprintln!("prism: /ui logout refused: {e}");
                gw.send_ui(id, verb("error", ""));
            }
        },
        "turn.submit" | "turn.cancel" => turn(gw, id, user, device, &event, data).await,
        other => {
            // The auth gate runs BEFORE the call, reading the booth's persisted
            // interface_schema `auth` block — the same declaration face the peer leg
            // enforces, so a booth declares its requirement once for both legs.
            let bound = user.load(Ordering::Relaxed);
            if bound == 0 && gw.handler_requires_auth(other).await {
                eprintln!("prism: /ui event `{other}` refused: authentication required");
                gw.send_ui(id, verb("error", ""));
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
                Ok(aura_booth::call::Waited::Done(Ok(v))) => {
                    reply_frames(gw, id, Ok(vec![v]), other).await
                }
                Ok(aura_booth::call::Waited::Done(Err(e))) => {
                    eprintln!("prism: /ui event `{other}` answered an error: {e}");
                    gw.send_ui(id, verb("error", ""));
                }
                Ok(aura_booth::call::Waited::Pending(_)) => {
                    eprintln!("prism: /ui event `{other}`: this leg has no cold path");
                    gw.send_ui(id, verb("error", ""));
                }
                Err(e) => {
                    eprintln!("prism: /ui event `{other}`: {e}");
                    gw.send_ui(id, verb("error", ""));
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
        gw.send_ui(id, verb("error", ""));
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
        gw.send_ui(id, verb("error", ""));
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
        gw.send_ui(id, verb("error", ""));
        return;
    }
    // The deployment facts (endpoint, model, store) are read per submit: a venue without
    // them still serves the shell, and the refusal names what is missing.
    let config = match crate::turns::TurnConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("prism: /channel turn: {e}");
            gw.send_ui(id, verb("turn.error", ""));
            return;
        }
    };
    table.lock().unwrap_or_else(std::sync::PoisonError::into_inner).reserve(id);
    let realm = Arc::clone(&gw.engine.realm);
    let slots = crate::turns::ViewSlots {
        rack: UI_RACK.to_string(),
        text: UI_TEXT.to_string(),
    };
    let out = Arc::clone(gw);
    let send = move |ev: &str, a: Value| match ev {
        "turn.frames" => {
            let Some(frames) = a.get("frames").and_then(Value::as_array) else {
                eprintln!("prism: turn.frames carried no array");
                return;
            };
            for frame in frames {
                match serde_json::from_value::<Content<Value>>(frame.clone()) {
                    Ok(op) => out.send_ui(id, draw(op)),
                    Err(e) => eprintln!("prism: turn frame is not an Accrete operation: {e}"),
                }
            }
        }
        "turn.done" => out.send_ui(id, verb("turn.done", "")),
        "turn.error" => {
            eprintln!("prism: turn failed: {a}");
            out.send_ui(id, verb("turn.error", ""));
        }
        other => eprintln!("prism: the turn answered `{other}` on the renderer leg: {a}"),
    };
    tokio::spawn(crate::turns::run_turn(
        realm, table, config, id, session_id, content, Some(slots), send,
    ));
}

/// Deliver a frame list as `draw` frames, then the terminator. A frame that is not an
/// Accrete operation is named by index and SKIPPED: one typo still draws the rest, and the
/// log is where the fix starts.
async fn reply_frames(
    gw: &Arc<Gateway>,
    id: u64,
    frames: anyhow::Result<Vec<Value>>,
    event: &str,
) {
    match frames {
        Ok(frames) => {
            for (index, frame) in frames.into_iter().enumerate() {
                match serde_json::from_value::<Content<Value>>(frame) {
                    Ok(op) => {
                        gw.send_ui(id, draw(op));
                    }
                    Err(e) => {
                        eprintln!("prism: {event} frame #{index} is not an Accrete operation: {e}")
                    }
                }
            }
            // The terminator carries no payload: the answer IS the frames above.
            gw.send_ui(id, verb(&format!("{event}.result"), ""));
        }
        Err(e) => {
            // The renderer's vocabulary has no free-text field, so the REASON goes to the
            // log and the wire gets the verb: inventing a field would be a second protocol.
            eprintln!("prism: {event} produced no frames: {e}");
            gw.send_ui(id, verb(&format!("{event}.error"), ""));
        }
    }
}

/// A verb frame: `ev != "draw"`, so a renderer ignores it — the plane speaks without
/// teaching the page a second vocabulary.
fn verb(ev: &str, sender: &str) -> Message<Value> {
    Message {
        ev: ev.to_string(),
        sender: sender.to_string(),
        created: None,
        content: Vec::new(),
    }
}

fn draw(op: Content<Value>) -> Message<Value> {
    Message { ev: "draw".to_string(), sender: String::new(), created: None, content: vec![op] }
}