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


/// Whose resources a remote call may touch. The realm recorded the owner when it dispatched the
/// call (stamped from the turn's envelope); this plane only parses the text it is given. It does
/// NOT look at the instance key: that key's shape is ADR-0018's vocabulary and belongs to the
/// layer that defines it, not to the resource door — the same door was reading it twice, once per
/// arm, and both copies broke as soon as a mount instance carried a call instead of a turn.
async fn owner_of_call(realm: &aura_realm::SharedRealm, call_id: &str) -> Option<okm_core::Uuid7> {
    let owner = {
        realm
            .lock()
            .await
            .pending_remote
            .get(call_id)
            .and_then(|p| p.owner.clone())
    };
    owner.and_then(|text| text.parse::<okm_core::Uuid7>().ok())
}

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

/// The names the pool will match on: `{user}:{label}`, or the bare label for a node that
/// belongs to the installation rather than to a user.
///
/// The prefix is added HERE, at the execution layer, and not in the registry: the operator
/// types `MyLaptop`, the registry stores `MyLaptop`, and only the address the venue matches on
/// carries the owner. Renaming a machine therefore renames its name, not its history.
fn prefixed(labels: &[String], owner: &str) -> Vec<String> {
    if owner.is_empty() {
        return labels.to_vec();
    }
    labels.iter().map(|l| format!("{owner}:{l}")).collect()
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

    let (node_alias, carriers, class, machine) = match stream.next().await {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<Frame>(&text)? {
            Frame::Register {
                node_alias,
                carriers,
                class,
                machine,
            } if node_alias == path_alias => (node_alias, carriers, class, machine),
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
    // (labels, owner) of the token that admitted this connection: the names the operator gave
    // the machine in the admin UI, and whose they are. Empty in `open` posture — there is no
    // token, so there are no names, and the node is simply unplaced.
    let mut named: Vec<String> = Vec::new();
    let mut owner = String::new();
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
            let verdict = gw.nodes().verdict(&node_alias, credential, &machine)?;
            match verdict {
                crate::nodes::Verdict::Registered { labels, owner: who } => {
                    named = labels;
                    owner = who;
                }
                crate::nodes::Verdict::Pending => {
                    let reason = format!(
                        "no approved credential for '{node_alias}' yet — this token's digest is \
                         queued; approve it with POST /admin/nodes/{{alias}}/approve"
                    );
                    refuse(&mut sink, reason.clone()).await;
                    anyhow::bail!("register refused: {reason}");
                }
                crate::nodes::Verdict::Conflict { why } => {
                    let reason = format!("'{node_alias}' refused: {why}");
                    refuse(&mut sink, reason.clone()).await;
                    anyhow::bail!("register refused: {reason}");
                }
            }
        }
    }

    sink.send(Message::Text(serde_json::to_string(&Frame::Registered)?))
        .await?;

    // A node that may be admitted becomes CALLABLE (ADR-0022): the vocabulary is the
    // deployment's, the alias came from the approved row (or, in `open`, from the dial itself),
    // and the mounts are their product. Idempotent — a boot-time row adds nothing here, a row
    // approved after boot (or an `open` alias) gets its types now instead of at the next restart.
    let minted = gw.ensure_alias_mounts(&node_alias).await;
    if !minted.is_empty() {
        eprintln!("effector mount: '{node_alias}' is callable as {minted:?}");
    }

    // The language↔node fact (ADR-0001): the frame announces the carriers this
    // node was built with, the manifest declares what its mounts expect. Drift
    // is stated, never silent (the same disclosure rule the alias-replacement
    // line above follows); it is NOT a refusal — a call into an un-carried
    // language answers with the carrier's own error value per call.
    if let Some(line) = gw.with_mount_table(|table| {
        crate::mounts::dial_in_note(table, &node_alias, &carriers)
    }) {
        eprintln!("{line}");
    }
    // What contains this node (ADR-0003 §1): the one fact that decides whether it may be handed code
    // nobody reviewed. Stated at the dial like the alias and the carriers, because an operator
    // reading a log needs to know which of these machines can run a turn's fresh script.
    eprintln!(
        "effector mount: '{node_alias}' is class `{}`{}",
        class.as_str(),
        if class == effector_protocol::NodeClass::Container {
            ""
        } else {
            " — a call that requires a container is refused here, in words"
        }
    );

    // Writer channel: realm calls push frames; this task owns the sink.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Frame>();
    let entry = aura_realm::ActuatorConn { sender: tx.clone(), peer };
    {
        // The venue's pool, not this realm's own map (aura ADR-0053): every realm the engine
        // creates borrows it, so a job dispatched a moment later can find this node.
        let pool = {
            let r = realm.lock().await;
            r.pool.clone()
        };
        let record = aura_realm::pool::NodeRecord {
            alias: node_alias.clone(),
            conn: entry.clone(),
            carriers: carriers.clone(),
            class,
            machine: machine.clone(),
            // Names become addresses here, and only here: the user id is prepended so two
            // users' `MyLaptop` cannot collide in a placement, and the registry keeps the names
            // bare so a rename stays a rename (aura ADR-0053, effector ADR-0006).
            labels: prefixed(&named, &owner),
        };
        let admitted = pool.insert(record);
        if let Some(old) = &admitted.displaced {
            // Alias replacement (in `open` posture a restarted container
            // must be able to reclaim its name from a stale registration).
            // Allowed — but never silent (ADR-0015 §7).
            eprintln!(
                "effector mount: alias '{node_alias}' REPLACED — old peer {} displaced by new peer {peer:?}",
                old.conn.peer
            );
        }
        if !admitted.same_machine.is_empty() {
            // One box, two names — or a serial two nodes both claim. Nothing here decides which
            // reading is right; saying it is the whole job (effector ADR-0006).
            eprintln!(
                "effector mount: machine '{}' is also connected as {}",
                if machine.is_empty() { "(unidentified)" } else { &machine },
                admitted.same_machine.join(", ")
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
            // THE RESOURCE DOOR (ADR-0003 §7, ADR-0049). The node's script asked for one kind and
            // this is what reaching it takes; WHO is asking comes from here, because devices and
            // their tenants are this plane's registry — the booth that runs the turn never holds a
            // second copy of that mapping. The answer is the venue's, so the request is a CALL into
            // the booth instance that dispatched this remote call, and it is SPAWNED for the same
            // reason the ctx bridge above is: its result arrives as a frame only this loop can
            // read.
            Frame::MaterialRequest {
                call_id,
                kind,
                write,
            } => {
                let owner = owner_of_call(&realm, &call_id).await;
                let registry = { realm.lock().await.resources.clone() };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let refusal = |reason: String| Frame::MaterialFailed {
                        call_id: call_id.clone(),
                        kind: kind.clone(),
                        reason,
                    };
                    // The registry is the venue's own table (ADR-0048), not a booth's: no instance
                    // has to be in flight, and nothing about a chat venue leaks into a build one.
                    let Some(owner) = owner else {
                        tx.send(refusal(
                            "no owner is recorded for this call — the realm stamped none when it \
                             dispatched, so this plane cannot say whose resource is meant"
                                .into(),
                        ))
                        .ok();
                        return;
                    };
                    let frame = match registry.material(&owner.to_string(), &kind, write) {
                        Ok(material) => Frame::Material {
                            call_id: call_id.clone(),
                            kind: kind.clone(),
                            base: material.base,
                            credential: material.credential,
                            mode: material.mode.as_str().to_string(),
                        },
                        Err(e) => refusal(format!("material: {e:#}")),
                    };
                    tx.send(frame).ok();
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
            let pool = {
                let r = realm.lock().await;
                r.pool.clone()
            };
            // Identity-checked inside the pool: a reconnect that took the alias over keeps it.
            pool.remove(&alias, &channel);
        });
    }
}
