//! Effector dial-in, mounted at `/effector/<alias>` (ADR-0017 §8): the
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
///
/// `token` is what arrived on the dial (`?token=…`). It is the node's authority (ADR-0003): the
/// plane hashes it and asks the registry whether that digest is approved for the alias. Nothing
/// about identity travels inside a frame — the frame says what the node IS.
pub(crate) async fn serve_mounted(
    gw: &Gateway,
    path_alias: String,
    token: Option<String>,
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

    // The node speaks first, and only to describe itself: alias + carriers. Its authority rode
    // the dial, so there is no challenge to answer and no signature to check — a proof that must
    // travel inside a frame is a credential inside a frame, and the connection already carries it.
    let credential = token.as_deref().map(crate::nodes::token_credential);

    let (node_alias, carriers) = match stream.next().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<Frame>(&text)? {
            Frame::Register {
                node_alias,
                carriers,
            } if node_alias == path_alias => (node_alias, carriers),
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
                "effector mount: '{node_alias}' dialed {}; posture `open` — registry not consulted",
                if credential.is_some() {
                    "with a token"
                } else {
                    "without a token"
                }
            );
        }
        crate::nodes::Posture::Required => {
            let credential = match credential.as_deref() {
                Some(c) => c,
                None => {
                    let reason = format!(
                        "no token on the dial for '{node_alias}' — this plane is in `required` \
                         posture; mint one with POST /admin/nodes and pass it as `?token=…`"
                    );
                    refuse(&mut sink, reason.clone()).await;
                    anyhow::bail!("register refused: {reason}");
                }
            };
            let verdict = gw.nodes().verdict(&node_alias, credential)?;
            match verdict {
                crate::nodes::Verdict::Registered => {}
                crate::nodes::Verdict::Pending => {
                    let reason = format!(
                        "no approved credential for '{node_alias}' yet — this token's digest is \
                         queued; approve it with POST /admin/nodes/{{alias}}/approve"
                    );
                    refuse(&mut sink, reason.clone()).await;
                    anyhow::bail!("register refused: {reason}");
                }
                crate::nodes::Verdict::Conflict => {
                    let reason = format!(
                        "another token holds the approved slot for '{node_alias}' — an enrolled \
                         node is never silently replaced; revoke it first"
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
            // The node's answer to a stop (aura ADR-0047 stage two). Nothing waits for
            // it — the caller's terminal value is already decided — so the honest half
            // is recorded here: `landed: false` means the node could not reach the work
            // and that call kept running. Silence must not read as "it stopped".
            // THE RESOURCE DOOR (ADR-0003 §7). The node names a resource and asks for material for
            // ONE transfer; whose resource it is comes from here, because devices and their owners
            // are this plane's registry — the booth that runs the turn never holds a second copy
            // of that mapping. The answer is the venue's, so the request is a CALL into the booth
            // instance that dispatched this remote call, and it is SPAWNED for the same reason the
            // ctx bridge above is: its result arrives as a frame only this loop can read.
            Frame::MaterialRequest {
                call_id,
                kind,
                seq,
                write,
            } => {
                let target = {
                    let r = realm.lock().await;
                    r.pending_remote.get(&call_id).map(|p| p.instance.clone())
                };
                let owner = target
                    .as_ref()
                    .and_then(|inst| match &inst.key {
                        // The envelope addresses a booth by DEVICE (ADR-0018), so the instance key
                        // is the device text a page's connection was identified by.
                        aura_booth::InstanceKey::Named(device) => {
                            device.parse::<okm_core::Uuid7>().ok()
                        }
                        aura_booth::InstanceKey::Singleton => None,
                    })
                    .and_then(|device| gw.registry().owner_of(device).ok());
                let registry = { realm.lock().await.resources.clone() };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let refusal = |reason: String| Frame::MaterialFailed {
                        call_id: call_id.clone(),
                        kind: kind.clone(),
                        seq,
                        reason,
                    };
                    // The registry is the venue's own table (ADR-0048), not a booth's: no instance
                    // has to be in flight, and nothing about a chat venue leaks into a build one.
                    let (Some(_instance), Some(owner)) = (target, owner) else {
                        tx.send(refusal(
                            "no turn in flight owns this call, or its device has no owner".into(),
                        ))
                        .ok();
                        return;
                    };
                    let frame = match aura_realm::resources::Id::new(&owner.to_string(), &kind, seq)
                    {
                        Ok(id) => match registry.material(&id, write) {
                            Ok(material) => Frame::Material {
                                call_id: call_id.clone(),
                                kind: kind.clone(),
                                seq,
                                url: material.url,
                                credential: material.credential,
                                transport: kind.clone(),
                                mode: material.mode.as_str().to_string(),
                                at: material.at,
                            },
                            Err(e) => refusal(format!("material: {e:#}")),
                        },
                        Err(e) => refusal(format!("{e:#}")),
                    };
                    tx.send(frame).ok();
                });
            }
            // The node published; the venue records the version and advances the pointer. Fire and
            // follow: nothing on the wire waits for this, but a failure is logged rather than
            // swallowed — a lost version is a lost undo.
            Frame::Synced {
                call_id,
                kind,
                seq,
                reference,
            } => {
                let target = {
                    let r = realm.lock().await;
                    r.pending_remote.get(&call_id).map(|p| p.instance.clone())
                };
                let owner = target
                    .as_ref()
                    .and_then(|inst| match &inst.key {
                        aura_booth::InstanceKey::Named(device) => {
                            device.parse::<okm_core::Uuid7>().ok()
                        }
                        aura_booth::InstanceKey::Singleton => None,
                    })
                    .and_then(|device| gw.registry().owner_of(device).ok());
                let registry = { realm.lock().await.resources.clone() };
                let path_alias = path_alias.clone();
                tokio::spawn(async move {
                    let (Some(_instance), Some(owner)) = (target, owner) else {
                        eprintln!("prism: node '{path_alias}' published {kind}/{seq} with no owner — not recorded");
                        return;
                    };
                    let id = match aura_realm::resources::Id::new(&owner.to_string(), &kind, seq) {
                        Ok(id) => id,
                        Err(e) => {
                            eprintln!("prism: version of {kind}/{seq} not recorded: {e:#}");
                            return;
                        }
                    };
                    // Blocking store work on the runtime's thread is not held by any lock: the
                    // registry was cloned out of the realm above.
                    match registry.published(&id, &call_id, &reference, crate::identity::now_ms()) {
                        Ok((version, previous)) => {
                            println!("prism: {kind}/{seq} published v{version} (was v{previous})");
                        }
                        Err(e) => eprintln!("prism: version of {kind}/{seq} not recorded: {e:#}"),
                    }
                });
            }
            Frame::Interrupted { call_id, landed } => {
                if !landed {
                    eprintln!(
                        "prism: node '{path_alias}' could not reach call {call_id} — no \
                         carrier on it can act on a stop, so that call kept running"
                    );
                }
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
