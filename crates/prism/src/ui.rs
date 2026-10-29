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
//! design (its ADR 0003 operation channel), so one socket still carries ONE frame shape.
//!
//! What this cut does: `init` (the layout, delivered as ordinary `create`/`set`/… frames),
//! the connect answer, and the codec handshake. Uplinks other than `init` are logged and
//! dropped — they belong to the application's booth dispatch, which is the next cut, not
//! this one. Nothing here invents a second protocol: every byte is `content`'s.

use std::sync::Arc;

use content::codec::ActiveCodec;
use content::{Content, Message, Outflow};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message as Ws;
use tokio_tungstenite::WebSocketStream;

use crate::code_export::Prefixed;
use crate::Gateway;

/// The renderer's path. Deliberately not `/ws`: the programmatic leg keeps the `{ev, args}`
/// shape, and one path never carries two shapes.
pub const UI_PATH: &str = "/ui";

/// Serve one renderer connection to completion.
pub(crate) async fn serve(
    gw: Arc<Gateway>,
    ws: WebSocketStream<Prefixed>,
    codec: ActiveCodec,
    device: u64,
) -> anyhow::Result<()> {
    let (mut write, mut read) = ws.split();

    // The connect answer: identity on the wire, in the renderer's shape. `ev != "draw"`, so
    // a renderer ignores it; a client that cares (a login form, a test) reads `sender`.
    let hello = Message {
        ev: "connected".to_string(),
        sender: format!("device:{device}"),
        created: None,
        content: Vec::new(),
    };
    send(&mut write, &codec, &hello).await;

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
                // A frame we cannot read is the client's bug, and the renderer's payloads
                // are not ours to guess. Say so on the log, keep the socket up.
                eprintln!("prism: /ui uplink not decodable as Outflow: {e}");
                continue;
            }
        };
        match uplink.event.as_str() {
            "init" => {
                let frames = layout_frames(&gw).await;
                match frames {
                    Ok(frames) => {
                        let mut delivered = 0usize;
                        for frame in frames {
                            match serde_json::from_value::<Content<Value>>(frame.clone()) {
                                Ok(op) => {
                                    let msg = Message {
                                        ev: "draw".to_string(),
                                        sender: String::new(),
                                        created: None,
                                        content: vec![op],
                                    };
                                    send(&mut write, &codec, &msg).await;
                                    delivered += 1;
                                }
                                // One malformed frame is named with its index and SKIPPED:
                                // a layout with a typo still draws the rest, and the log is
                                // where the fix starts.
                                Err(e) => eprintln!(
                                    "prism: layout frame #{delivered} is not an Accrete operation: {e}"
                                ),
                            }
                        }
                        // The terminator: an empty `init.result` frame. A renderer ignores
                        // it (ev != draw); a client that wants to know the layout is done
                        // waits for it.
                        let ack = Message {
                            ev: "init.result".to_string(),
                            sender: String::new(),
                            created: None,
                            content: Vec::new(),
                        };
                        send(&mut write, &codec, &ack).await;
                    }
                    // No source (or a source that failed): the renderer still gets an
                    // answer — an empty `init.error` frame, which it ignores and a client
                    // reads. The REASON goes to the log: the Accrete wire has no free-text
                    // field, and inventing one would be a second vocabulary.
                    Err(e) => {
                        eprintln!("prism: init failed: {e}");
                        let failed = Message {
                            ev: "init.error".to_string(),
                            sender: String::new(),
                            created: None,
                            content: Vec::new(),
                        };
                        send(&mut write, &codec, &failed).await;
                    }
                }
            }
            other => {
                // Application uplinks (a form submit, a nav) are ordinary events, but their
                // dispatch onto the booth plane is the next cut. Dropping them loudly beats
                // answering in a shape nobody asked for.
                eprintln!("prism: /ui uplink `{other}` has no dispatch yet — dropped");
            }
        }
    }
    Ok(())
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

async fn send<S>(write: &mut S, codec: &ActiveCodec, msg: &Message<Value>)
where
    S: SinkExt<Ws> + Unpin,
    <S as futures_util::Sink<Ws>>::Error: std::fmt::Display,
{
    match codec.encode(msg) {
        Ok(bytes) => {
            if let Err(e) = write.send(Ws::Binary(bytes)).await {
                eprintln!("prism: /ui write failed: {e}");
            }
        }
        Err(e) => eprintln!("prism: /ui encode failed: {e}"),
    }
}