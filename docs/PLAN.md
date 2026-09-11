# PLAN

Design lives in the wiki (stateless-agent-architecture.md); this file only sequences phases.

## Milestone A — WS gateway

- [ ] Phase 0 — Workspace skeleton: `crates/{gateway,protocol,cli,config}`; protocol contract: turn submit / stream frames / cancel / session lifecycle, one message schema for all clients.
- [ ] Phase 1 — WS gateway on Aura: resident component; client connections pin here (never on Gravity); auth + parse + turn submission as realm events; streaming back via event subscription (SSE-over-WS frames).
- [ ] Phase 2 — CLI wrapping WS: thin client — every CLI command maps to the same WS protocol; no second RPC surface. Local Krystallizer mode is Gravity's CLI, not Prism's.

Deferred gates:

- Multi-tenant auth (keys/accounts): single-user first; add auth layers only with the first external consumer.
- HTTP/SSE secondary endpoint: rejected by design (protocol unification); revisit only with a hard client constraint that cannot do WS.
