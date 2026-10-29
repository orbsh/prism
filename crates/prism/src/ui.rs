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
pub const UI_PATH: &str = "/ui";

/// The instance key every application event on this leg lands on.
const UI_INSTANCE: &str = "ui";

/// One renderer connection: its writer channel and the auth field a `login` writes. The
/// table lives on [`Gateway`] so the acts that must reach every leg (revocation) see it.
pub(crate) struct UiConn {
    pub tx: mpsc::UnboundedSender<Message<Value>>,
    pub device: u64,
    pub user: Arc<AtomicU64>,
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
        "init" => reply_frames(gw, id, layout_frames(gw).await, "init").await,
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

/// The layout frames for this deployment: the boot-read document, or a global booth's answer.
async fn layout_frames(gw: &Arc<Gateway>) -> anyhow::Result<Vec<Value>> {
    let Some(source) = gw.init_source() else {
        anyhow::bail!(
            "no init source configured — set PRISM_INIT=file:<path> or event:<type>/<handler>"
        );
    };
    match source {
        crate::init::InitSource::File(frames) => Ok(frames),
        crate::init::InitSource::Event { booth_type, handler } => gw.call_global(&booth_type, &handler).await,
    }
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