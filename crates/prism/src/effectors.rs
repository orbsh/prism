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

/// Answer a refused claim with the reason, then let the caller close. Best-effort by
/// construction: the operator's evidence is worth one frame, never a hang.
async fn refuse<S>(sink: &mut S, reason: String)
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Debug,
{
    let frame = Frame::Refused { reason };
    if let Ok(text) = serde_json::to_string(&frame) {
        let _ = sink.send(Message::Text(text)).await;
    }
}

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
    let peer = ws
        .get_ref()
        .inner
        .peer_addr()
        .unwrap_or_else(|_| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));
    let (mut sink, mut stream) = ws.split();

    // THE HOST SPEAKS FIRST: a fresh nonce, in either posture. One wire shape means a node
    // never has to know how it is being trusted, and a replayed Register cannot answer a
    // later connection.
    let nonce = effector_protocol::nonce();
    sink.send(Message::Text(
        serde_json::to_string(&Frame::Challenge { nonce: nonce.clone() })?,
    ))
    .await?;

    // Registration: alias + carriers + proof. The frame's alias must equal the path's (the
    // URL segment is the claim this socket arrived under) and the signature must verify
    // against the public key the frame states — that is possession, and it is checked in
    // BOTH postures: `open` means "no registry", never "no proof".
    let (node_alias, carriers, public_key) = match stream.next().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<Frame>(&text)? {
            Frame::Register { node_alias, carriers, public_key, signature }
                if node_alias == path_alias =>
            {
                if let Err(why) =
                    effector_protocol::verify_register(&public_key, &nonce, &node_alias, &signature)
                {
                    refuse(&mut sink, format!("signature: {why}")).await;
                    anyhow::bail!("register refused for '{node_alias}': {why}");
                }
                (node_alias, carriers, public_key)
            }
            // A claim that contradicts its own URL is refused like any other, and STATED: the
            // node operator has one socket and no log on this machine, so a reason that only
            // exists in the host's log is a reason nobody can act on.
            Frame::Register { node_alias, .. } => {
                let reason = format!(
                    "register alias '{node_alias}' does not match the mount path '{path_alias}' — \
                     the URL is the claim this socket arrived under"
                );
                refuse(&mut sink, reason.clone()).await;
                anyhow::bail!("register refused: {reason}")
            }
            other => {
                let reason = format!("expected Register, got {other:?}");
                refuse(&mut sink, reason.clone()).await;
                anyhow::bail!("register refused: {reason}")
            }
        },
        other => anyhow::bail!("bad register frame: {other:?}"),
    };

    // THE REGISTRY, when the deployment asks for trust. `open` records nothing and says so;
    // `required` maps the verdict onto the connection, and a refusal is STATED before the
    // socket closes (an operator on the node machine has no other way to learn the reason).
    match gw.posture() {
        crate::nodes::Posture::Open => {
            eprintln!(
                "effector mount: '{node_alias}' key {} verified; posture `open` — registry not consulted",
                &public_key[..public_key.len().min(16)]
            );
        }
        crate::nodes::Posture::Required => {
            let verdict = gw.nodes().verdict(&node_alias, &public_key)?;
            match verdict {
                crate::nodes::Verdict::Registered => {}
                crate::nodes::Verdict::Pending => {
                    let reason = format!(
                        "no approved key for '{node_alias}' yet — this claim is queued; \
                         approve it with POST /admin/nodes/{{alias}}/approve"
                    );
                    refuse(&mut sink, reason.clone()).await;
                    anyhow::bail!("register refused: {reason}");
                }
                crate::nodes::Verdict::Conflict => {
                    let reason = format!(
                        "another key holds the approved slot for '{node_alias}' — an enrolled \
                         key is never silently replaced; revoke it first"
                    );
                    refuse(&mut sink, reason.clone()).await;
                    anyhow::bail!("register refused: {reason}");
                }
            }
        }
    }

    sink.send(Message::Text(serde_json::to_string(&Frame::Registered)?))
        .await?;

    // The language↔node fact (ADR-0001): the frame announces the carriers this
    // node was built with, the manifest declares what its mounts expect. Drift
    // is stated, never silent (the same disclosure rule the alias-replacement
    // line above follows); it is NOT a refusal — a call into an un-carried
    // language answers with the carrier's own error value per call.
    if let Some(line) = crate::mounts::dial_in_note(&node_alias, &carriers) {
        eprintln!("{line}");
    }

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
                // Ctx bridge over the wire: resolve against the instance the
                // enclosing remote call was routed to, then reply on this
                // connection's writer. The resolution is SPAWNED, never
                // awaited inline: it parks on a hot realm call whose
                // `Frame::Result` can only be delivered by THIS reader loop,
                // and a ctx target on this same node (the ordinary case —
                // one node, several mounts) would wait for a message only
                // this loop can read: a self-deadlock up to the call
                // deadline. The loop's job is correlation, not resolution.
                let realm = realm.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let instance = {
                        let r = realm.lock().await;
                        r.pending_remote
                            .get(&call.call_id)
                            .map(|p| p.instance.clone())
                    };
                    let outcome = match instance {
                        Some(inst) => {
                            aura_engine::host_wire::resolve_host_call(&realm, &inst, &call.op).await
                        }
                        None => Err(
                            "unknown call_id: the enclosing remote call is not in flight".into(),
                        ),
                    };
                    tx.send(Frame::Host(HostFrame::Result(
                        effector_protocol::HostResult {
                            host_call_id: call.host_call_id,
                            outcome,
                        },
                    )))
                    .ok();
                });
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
