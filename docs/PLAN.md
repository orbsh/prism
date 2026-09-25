# PLAN

Design lives in the wiki (stateless-agent-architecture.md); this file only sequences phases.

## Milestone A — WS gateway

- [ ] Phase 0 — Workspace skeleton: `crates/{gateway,protocol,cli,config}`; protocol contract: turn submit / stream frames / cancel / session lifecycle, one message schema for all clients.
- [ ] Phase 1 — WS gateway on Aura: resident component; client connections pin here (never on Gravity); auth + parse + turn submission as realm events; streaming back via event subscription (SSE-over-WS frames).
- [ ] Phase 1.5 — Scope note (no code): WS appears in exactly two stateful places — Prism↔clients (entry, this repo) and the control plane↔remote-Probe outbound long connections (hosted on Aura's connection plane, Phase 6.5 of aura; not this repo's gateway). Inside the realm there are no connections — Gravity↔Prism/Probe are realm events. No work in this repo; recorded to prevent protocol drift.
- [ ] Phase 2 — CLI wrapping WS: thin client — every CLI command maps to the same WS protocol; no second RPC surface. Local Krystallizer mode is Gravity's CLI, not Prism's.
- [ ] Phase 1.8 — Node identity, trust plane (aura ADR-0015 Update 2026-09-25 — attribution lands HERE, not in aura): the `identity` config field (no default; `required` | `open`), the ed25519 keypair handshake (`challenge{nonce}` → signed `register` → `registered`/`pending`/`conflict`/`unauthenticated`), the node registry (`{alias, public_key, status, created_at}` in prism's own okm instance), and the four curl approval endpoints under `/admin` (`POST /nodes`, `GET /nodes`, `POST /nodes/{alias}/approve`, `DELETE /nodes/{alias}`). Step ③ (fold records under the account) rides Phase 1's auth. Aura's landed residue (replacement discipline + startup disclosure) needs no change here — it is already true under whatever posture the gateway mounts.
- [ ] Phase 1.9 — Static code export (aura ADR-0027): `GET /code/{sha256}` serving the meta-plane `CodeBlob` rows as plain immutable downloads (Cache-Control: immutable; content only ever at its own `/{hash}` path — the URL is self-verifying by construction), same-process read of aura's storage (prism is a resident component). No auth by default: the hash is the capability, the URL only appears inside control-plane-signed frames; confidentiality is a deployment option (private static source, or signed URLs with CDN cache-key normalization so signatures do not defeat caching). No per-node code ACL — that would extend ADR-0015 node identity, not mint a second authorization surface. This endpoint is the remote-probe delivery prerequisite (remote actor code rides `CodeRef{url, sha256}` only).

Deferred gates:

- Multi-tenant auth (keys/accounts): single-user first; add auth layers only with the first external consumer.
- HTTP/SSE secondary endpoint: rejected by design (protocol unification); revisit only with a hard client constraint that cannot do WS.
