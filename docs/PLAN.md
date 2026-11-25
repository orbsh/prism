# PLAN

The model lives in `docs/MODELING.md`, the decisions in `docs/adr/`, the architecture this repo serves in the wiki. This file is the index: which phase landed when, what is left, and where the reason is written.

## Milestone A — the WS gateway

- [x] 0 — Workspace skeleton; one message schema `{ev, args}` both directions, CBOR default and JSON debug — 2026-09-25
- [x] 1 — The gateway on aura: identity, per-event auth enforcement, logout, revocation — 2026-09-25 … 2026-10-10 — [ADR-0017](adr/0017-prism-connection-plane.md)
- [x] 1.5 — Scope note: the WS appears in exactly two places (clients↔venue, plane↔node) — 2026-11-07, note only, no code
- [x] 1.8 — Node identity and the trust plane — 2026-09-26 … 2026-10-31 — [ADR-0017](adr/0017-prism-connection-plane.md) §6; the signed handshake was later replaced by a dial token, effector ADR-0003 §11
- [x] 1.9 — Static code export at `GET /code/{sha256}` — 2026-09-26 — aura ADR-0027
- [x] 1.10 — The renderer leg: the bundle is the origin, `/channel` speaks fluxen's wire — 2026-10-29 … 2026-11-07 — [ADR-0017](adr/0017-prism-connection-plane.md) §8
- [x] 2 — A CLI wrapping the WS — CLOSED 2026-11-09 as moot: the venue ships no client, and gravity's binary is the client — gravity ADR-0002
- [x] 3 — The turn left this repo — 2026-10-30 — gravity ADR-0007, 0008, 0009
- [x] 4 — Identity: a token is the credential, a device is a name, one clock — 2026-10-31 — [ADR-0017](adr/0017-prism-connection-plane.md), `MODELING.md`
- [x] 5 — The auth surface: the login is the venue's, the proof is a booth's, the surface is one reserved region — 2026-11-06 — [ADR-0017](adr/0017-prism-connection-plane.md) §2, `MODELING.md`
- [x] 6 — A refusal opens the login surface (`ui:auth.required {event}`) — 2026-11-06
- [x] 7 — User addressing: an emit names `device` or `user` — 2026-11-09 — [ADR-0018](adr/0018-device-is-not-a-channel.md) §3
- [x] 8 — The account device surface: `devices` and `alias` — 2026-11-09
- [x] 9 — The transport's probe on `/channel`: `__ping__` answered `__pong__` before any surface logic, in the pinned encoding, through the single writer; not dispatched (no booth is named like a probe) and not activity (a page probing every five seconds must not hold the idle window open) — 2026-11-25 — [ADR-0017](adr/0017-prism-connection-plane.md) §4, fluxen ADR 0044

## Next

Nothing. OAuth2's redirect leg is out of scope by ruling: it needs a renderer "leave this page" operation (`content::Content` has none) and a callback prefix the venue does not reserve.

## Deferred

- Multi-tenant auth (keys and accounts) — single user first; add layers with the first external consumer
- An HTTP/SSE secondary endpoint — rejected by design (one protocol); revisit only for a hard client constraint that cannot do WS

## Where to read the reasons

- the venue's model: participants, what addresses what, the connection lifecycle, islands — `docs/MODELING.md`
- the connection plane, identity, and the renderer leg — [ADR-0017](adr/0017-prism-connection-plane.md)
- what a device is not — [ADR-0018](adr/0018-device-is-not-a-channel.md)
- the page frame is a file — [ADR-0019](adr/0019-page-frame-is-a-file.md)