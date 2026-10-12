//! Effector dial-in, mounted at `/effector/<alias>` (ADR-0017 §6): the
//! effectors' OUTBOUND WS connections terminate here — prism is the
//! connection plane — and register into the realm's effector table by
//! node alias. Semantics are aura's dial-in gateway unchanged (the
//! mount moves the door, not the room): `Frame::Register` first, then
//! Result correlation against `pending_remote` and ctx-bridge Host
//! frames resolved through `aura_engine::host_wire`.
//!
//! Path-vs-frame identity: the URL segment IS the alias claim, and the
//! Register frame repeats it — the two must agree (one fact, stated
//! twice) or the connection is refused: a path that admitted any alias
//! would make the URL a lie on every subsequent frame's logs. The
//! trust POSTURE is not checked here — handshake execution (the
//! challenge / signed register ride, Phase 1.8 step 2's second half)
//! is what consults the node registry; this module is the home it
//! mounts into.
//!
//! The presence guard is the same shape as aura's (replacement
//! discipline, ADR-0015 §7): the registration dies with the connection
//! task by ANY exit path, identity-checked so a reconnect's newer
//! channel survives an old task's teardown.

use crate::{code_export, Gateway};
use aura_realm::SharedRealm;
use effector_protocol::{Frame, HostFrame};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

/// Serve one effector connection whose upgrade requested
/// `/effector/{alias}`. Runs the register handshake, then the
/// reader/writer pair, to connection end.
pub(crate) async fn serve_mounted(
    gw: &Gateway,
    path_alias: String,
    ws: tokio_tungstenite::WebSocketStream<code_export::Prefixed>,
) -> anyhow::Result<()> {
    let realm = gw.engine.realm.clone();
    // The peer before the split: SplitStream wraps a BiLock (no
    // get_ref) — the registry entry's peer must be captured here.
    let peer = ws.get_ref().inner.peer_addr().unwrap_or_else(|_| {
        "0.0.0.0:0".parse().expect("fallback peer parses")
    });
    let (mut sink, mut stream) = ws.split();

    // Registration: alias + carriers. The frame's alias must equal the
    // path's (the URL segment is the claim this socket arrived under).
    let (node_alias, _carriers) = match stream.next().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<Frame>(&text)? {
            Frame::Register { node_alias, carriers, .. } if node_alias == path_alias => {
                (node_alias, carriers)
            }
            Frame::Register { node_alias, .. } => {
                anyhow::bail!("register alias '{node_alias}' does not match the mount path '{path_alias}'")
            }
            other => anyhow::bail!("expected Register, got {other:?}"),
        },
        other => anyhow::bail!("bad register frame: {other:?}"),
    };
    sink.send(Message::Text(serde_json::to_string(&Frame::Registered)?))
        .await?;

    // Writer channel: realm calls push frames; this task owns the sink.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Frame>();
    let entry = aura_realm::ActuatorConn { sender: tx.clone(), peer };
    {
        let mut r = realm.lock().await;
        if let Some(old) = r.effectors.insert(node_alias.clone(), entry.clone()) {
            // Alias replacement (in `open` posture a restarted container
            // must be able to reclaim its name from a stale registration).
            // Allowed — but never silent (ADR-0015 §7).
            eprintln!(
                "effector mount: alias '{node_alias}' REPLACED — old peer {} displaced by new peer {peer:?}",
                old.peer
            );
        }
    }
    let _presence = PresenceGuard {
        realm: Some(realm.clone()),
        alias: node_alias.clone(),
        channel: tx.clone(),
    };

    // Writer task: drain the channel into the socket.
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if sink.send(Message::Text(serde_json::to_string(&frame)?)).await.is_err() {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    });

    // Reader loop: Result correlation + ctx-bridge Host frames.
    while let Some(msg) = stream.next().await {
        let Message::Text(text) = msg? else { continue };
        match serde_json::from_str::<Frame>(&text)? {
            Frame::Result(result) => {
                let mut r = realm.lock().await;
                if let Some(pending) = r.pending_remote.remove(&result.call_id) {
                    let _ = pending.reply.send(result.outcome);
                }
                // Unknown call_id: the caller timed out — drop the late
                // result (pending_calls' scan owns timeouts).
            }
            Frame::Host(HostFrame::Call(call)) => {
                // Ctx bridge over the wire: resolve against the instance
                // the enclosing remote call was routed to.
                let instance = {
                    let r = realm.lock().await;
                    r.pending_remote.get(&call.call_id).map(|p| p.instance.clone())
                };
                let outcome = match instance {
                    Some(inst) => {
                        aura_engine::host_wire::resolve_host_call(&realm, &inst, &call.op).await
                    }
                    None => Err("unknown call_id: the enclosing remote call is not in flight".into()),
                };
                tx.send(Frame::Host(HostFrame::Result(effector_protocol::HostResult {
                    host_call_id: call.host_call_id,
                    outcome,
                })))
                .ok();
            }
            other => anyhow::bail!("unexpected frame from effector: {other:?}"),
        }
    }
    // Connection gone (clean EOF): `_presence` drops with the function.
    Ok(())
}

/// Unregisters the node alias when the connection task ends. Drop
/// cannot await, so the lock-taking removal runs in a detached task;
/// the identity check keeps a reconnect's registration intact.
struct PresenceGuard {
    realm: Option<SharedRealm>,
    alias: String,
    channel: tokio::sync::mpsc::UnboundedSender<Frame>,
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        let Some(realm) = self.realm.take() else { return };
        let alias = std::mem::take(&mut self.alias);
        let channel = self.channel.clone();
        tokio::spawn(async move {
            let mut r = realm.lock().await;
            if r.effectors.get(&alias).is_some_and(|conn| conn.sender.same_channel(&channel)) {
                r.effectors.remove(&alias);
            }
        });
    }
}
