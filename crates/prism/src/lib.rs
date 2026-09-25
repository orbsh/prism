//! prism — the connection plane (ADR-0017 §1/§4), first landing: a WS
//! gateway hosting the Aura echo actor in every embedded language.
//!
//! Wire shape (the reduced echo scope, documented in README):
//! - `GET /ws`, `?protocol=json` selects the debug codec (absent = CBOR,
//!   fixed for the connection lifetime — ADR-0017 §4).
//! - ONE frame shape both directions: `{ev, args}`; the protocol does
//!   not encode direction, dispatch cannot branch on it.
//! - Inbound `{ev: "<type>", args}` → `engine.call` the actor named by
//!   `ev` (instance key `"ws"`, handler = event name — the multi-entry
//!   model) → the result returns to the CALLING connection as
//!   `{ev: "<type>.result", args}`; failures return `{ev: "error"}` as
//!   a value, never a socket close.
//! - `{ev: "broadcast", args}` → the frame fans out to EVERY live
//!   connection (the fan-out traversal shape of ADR-0017 §3, minus the
//!   auth sets). Prism is the outbound bridge: the connection table is
//!   the gateway's, actors never address connections (modeling.md).
//!
//! Device identity, login/connection sets, `/admin` upload, `/probe`
//! mount, `/assets` are the rest of ADR-0017 — deliberately not here;
//! README states the gap so it reads as scope, not as drift.

pub mod actors;

use aura_actor::{call::Waited, ActorType, InstanceId};
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

type Conns = Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Frame>>>>;

/// Shared gateway state: the engine + the live-connection table.
pub struct Gateway {
    pub engine: Engine,
    conns: Conns,
    next_id: AtomicU64,
}

impl Gateway {
    pub fn new(engine: Engine) -> Arc<Self> {
        Arc::new(Self {
            engine,
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

    /// Serve one accepted connection to completion. The codec is read
    /// from the handshake query BEFORE the upgrade consumes the
    /// request (accept_hdr's callback is the only legal peek).
    async fn handle_conn(self: Arc<Self>, stream: TcpStream) -> anyhow::Result<()> {
        let codec_slot: Arc<Mutex<Option<Codec>>> = Arc::default();
        let slot = Arc::clone(&codec_slot);
        let ws = tokio_tungstenite::accept_hdr_async(stream, move |req: &Request, resp| {
            let query = req.uri().query().unwrap_or("");
            *slot.lock().unwrap() = Some(Codec::from_query(query_param(query, "protocol")));
            Ok(resp)
        })
        .await?;

        // The callback ran during the handshake; `unwrap_or` only
        // guards a handshake that never reached `accept`.
        let codec = codec_slot.lock().unwrap().unwrap_or(Codec::Cbor);
        let (mut write, mut read) = ws.split();

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel::<Frame>();
        self.conns.lock().unwrap().insert(id, tx.clone());

        // Outbound pump: the single socket writer — replies and
        // fan-out share it. Removal on ANY exit path lives here (the
        // presence-guard lesson: an aborted task never runs epilogues,
        // so the map cleanup rides the sender's death: the table prunes
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

    /// The one dispatch ruling (echo scope): `ev` names an actor TYPE,
    /// instance key `"ws"`, handler = the event name (multi-entry
    /// model); `broadcast` is the only non-invoke verb. Replies are
    /// addressed to the CALLING connection; broadcast fans out.
    async fn dispatch(&self, id: u64, frame: Frame) {
        let Frame { ev, args } = frame;

        if ev == "broadcast" {
            self.broadcast_frame(Frame::new("broadcast", args));
            return;
        }
        if ev == "ping" {
            self.send_to(id, Frame::new("ping.result", serde_json::json!({"pong": true})));
            return;
        }

        let target = InstanceId { actor_type: ev.clone(), key: "ws".into() };
        let payload = match self.engine.call(target, &ev, args.clone()).await {
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

    /// Queue one frame to every live connection; a closed sender is
    /// pruned in the same pass. Sync: an unbounded send never blocks.
    fn broadcast_frame(&self, frame: Frame) {
        if let Ok(mut conns) = self.conns.lock() {
            conns.retain(|_, tx| tx.send(frame.clone()).is_ok());
        }
    }

    fn send_to(&self, id: u64, frame: Frame) {
        if let Ok(conns) = self.conns.lock() {
            if let Some(tx) = conns.get(&id) {
                let _ = tx.send(frame);
            }
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
    }
}
