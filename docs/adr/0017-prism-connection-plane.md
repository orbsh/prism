# 0017 — Prism connection plane: dual identity, the connection set, and the unified event protocol

> **Languages:** [English](0017-prism-connection-plane.md) (primary) · [中文](0017-prism-connection-plane.zh-CN.md)

**Status:** Accepted (2026-09-22) — design; implementation pending, see Consequences

## Context

Aura's integration story has no client entry point. Gravity/krystallizer booths exist and remote probes already dial in (Phase 3), but there is no unified surface a test client, a browser, or a delivered application can connect to. PLAN Phase 8 names Prism as the WS gateway hosted as an Aura-resident component ("client connections pin here, not on Gravity; turn delivery = realm events"), and ADR-0015's node-approval endpoints were explicitly deferred until "Prism's auth exists".

The design must serve more than chat. Fluxen applications (see fluxora's Envelope model — `receiver: Vec<Session>` wrapping a `sender`/`created`/`content` message) are a pattern for arbitrary applications: chat, CMS, dashboards, commerce, freely composed. The connection plane therefore cannot bake in chat assumptions such as "every connection belongs to a user".

Constraints carried over from earlier rulings:

- ADR-0015: node identity is an ed25519 key per probe node; approval is by public key through four curl endpoints that "later hang under the account (Prism auth)".
- okm key discipline: `Key` fields reject `String` (fixed-width only); open-ended names are runtime DATA resolved through registry tables (`#[kv_index(by_name)]`), never hand-rolled byte keys.
- The probe holds no storage and receives no data-plane credentials (2026-09-20 withdrawal) — Prism is an engine-bearing host (the Aura node itself) and may hold the account registry.
- Long-polling fallback was rejected as an architecture principle; the transport contract is WS-only.

## Decision

### 1. Prism is the connection plane, hosted as an Aura-resident component

A new connection-plane crate in the aura repo owns: WS accept/upgrade, route dispatch (`/probe/<alias>`, `/admin/...`, application routes, `/assets/...`), the identity model, and the event protocol codec. The protocol definition stays owned by the Prism side of the contract (per PLAN Phase 8), but the connection plane is Aura code and rides realm events for turn delivery. Gravity, krystallizer and delivered fluxen applications are booths reached *through* Prism, not wired into it.

### 2. Dual identity: device and account; authentication is a business-layer decision

The framework recognises exactly two identities:

- **Device identity** — on first connect the server assigns a `device_id`; the client persists it in localstorage. Every event defaults to the device identity as sender. Dashboards, browsing, add-to-cart and chat itself all work with no account at all.
- **Account identity** — a `login` event (username + password) binds the `device_id` to a `user_id`. After binding, the sender on that connection is the `user_id`; the device binding persists across reconnects (re-login restores it).

The localstorage `device_id` is the anchor the framework maintains: the only framework duty is the device↔user binding record. Whether a given event requires an account is a business decision — each booth type declares per event whether it needs an authenticated sender; the framework only makes "is the current sender a device or a user" queryable and returns a standard error when a required-auth event meets an anonymous sender. Commerce declares it on "place order", chat may never declare it.

There is no idle eviction of unauthenticated connections: public services legitimately live on anonymous connections.

### 3. One connection set; auth state is a per-connection field

The gateway holds ONE set of connections. Each connection carries its own auth state as a field: anonymous (`None`) until a successful `login` binds it to a `user_id`. Group fan-out (broadcast to all, to authenticated-only, or to a declared subset) iterates the single set and selects by that field — the selection is a predicate evaluated once per fan-out, not a second collection kept in sync.

The earlier ruling (two sets, the connection physically moved on login) is superseded: two collections store the same fact twice, and every state change — login today, logout or server-side revocation whenever they land — must write both or the sets drift. One set with a per-connection field makes drift unrepresentable, and the fan-out traversal the two sets saved is not a hot path at connection scale.

Booths may declare whether they require authentication. Identity reaches a handler through the delivery PAYLOAD (the 2026-09-22 amendment below §7): prism wraps the sender envelope into the event args; `Ctx`, `Job`, `InstanceId` carry no identity fields. Identity is deliberately NOT the storage partition key either: a partition may be keyed by `channel_id` or any other business dimension (partitioning.md §2.1) — the address answers "who serially processes", the envelope answers "who initiated".

### 4. One event protocol, one field, JSON and CBOR

The wire carries ONE field — `ev` — in BOTH directions; the protocol does not encode direction. An event is an event: the client's `{"ev": "order.submit", ...}` and the server's `{"ev": "order.created", ...}` are the same shape. `emit`/`on` are per-end implementation details: on the aura side, an booth's `@on` declaration and `emit` call; on the client side, `ws.send` / `ws.on`. Prism is the natural extension of aura's event semantics to the user end — there is no client-action/server-event vocabulary split to translate through (aura ADR-0026, naming section) — the "action" word is retired altogether. The earlier draft's ruling (server→client frames distinguish themselves by their own field, never reusing the client's field) is satisfied a fortiori: one field, direction does not exist at the protocol layer — dispatch cannot branch on it.

Business operations (login, view manipulation, placing an order) are a client emitting an event with a business name — "action" is retired: a client action IS an emitted event, and the unified vocabulary says so exactly. Two encodings: CBOR (default) and JSON (debugging). Selection happens at handshake via a query parameter (`?protocol=json`); the codec is fixed for the lifetime of the connection.

The persistent WS stays connected; `login` is an ordinary event on it, not a separate HTTP round trip.

### 5. Admin prefix and booth upload

Under `/admin`: `POST` to upload/register booth code, plus the four node-approval endpoints from ADR-0015 (`POST /nodes`, `GET /nodes`, `POST /nodes/{alias}/approve`, `DELETE /nodes/{alias}`), now hanging under the account. Krystallizer and gravity each get a thin upload script (assemble JSON, POST) — the first script lives in this repo's `scripts/` to validate the endpoint, then is copied per repo.

### 6. Remote probe entry at `/probe/<alias>`

The existing probe dial-in gateway (register / Call / Result / Host frames) is mounted under `/probe/<alias>`. Semantics unchanged: probe dials out, realm keys `probes` by alias, residency identity travels as `session = "<booth_type>/<key>"`.

### 7. Account registry in okm, id-first access

A dedicated registry table: `user_id` (fixed-width binary) as key; username, nickname, password hash in the value; `#[kv_index(by_name)]` on `name` serving exactly one hot path — login-time exact lookup. Everything else (sessions, message senders, authorization checks) uses `user_id` only. The nickname is value data; the framework never resolves a user by nickname, and only account-management operations touch the username. The registry pattern follows `crates/realm/src/mq.rs` (proxy id key + name payload + index).

### 8. Fluxen integration: envelope shape and assets

View-manipulation operations are ordinary events on the same channel; the message envelope reuses fluxora's shape — `Envelope { receiver: Vec<Session>, message: { sender, created, content } }` — so delivered bricks carry the same structure across both systems. Static resources are served from `/assets/` as plain downloads carrying no event semantics.

## Honest semantic cost

- **Fan-out filters per connection.** With one set, every broadcast evaluates the auth predicate over all connections — O(all) instead of O(target). Accepted: connection counts are machine-scale small, and the alternative (two sets, moved on login) duplicated the auth fact and left every future revocation path a second collection to remember.
- **Unauthenticated connections are unmetered.** No idle eviction means an anonymous flood holds connections indefinitely. Mitigation belongs to deployment (connection caps per IP), not to the protocol.
- **Device identity is a convenience anchor whose loss is bounded by business design, not by the framework.** A stolen device_id impersonates the device's anonymous history. How much that matters is a per-application decision: the commerce pattern keeps the cart in localstorage and syncs/merges with the server only after login, so the pre-auth history's authority stays local. Account credentials are the real secret; the framework treats device identity as an addressable convenience identity, and applications decide what to entrust to it.
- **Dual encoding doubles the codec surface.** Every frame type has two serialization paths; protocol evolution must keep CBOR and JSON shape-compatible (same field sets, different physical encodings) or debug clients diverge from production ones.

## Consequences

- Integration tests gain a real entry point: a WS client speaks the event protocol end to end — upload booth via `/admin`, connect, `login`, drive events, observe realm events.
- Phase 8's "turn delivery = realm events" is now the binding contract for Prism, not a plan note.
- ADR-0015's step 3 (fold node approval into account auth) becomes implementable: the four endpoints exist and hang under the user registry.
- A later chat/commerce application defines its own per-event auth declarations; the framework ships with none beyond the `login` event itself.
- Implementation sequence: `crates/prism` (WS, identity, sets, codec) → `/admin` + upload endpoint + node-approval folding → `/probe/<alias>` mount → user registry table → `/assets/` + envelope-shaped view events.
