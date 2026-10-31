# 0017 — Prism connection plane: dual identity, the connection set, and the unified event protocol

> **Languages:** [English](0017-prism-connection-plane.md) (primary) · [中文](0017-prism-connection-plane.zh-CN.md)

**Status:** Accepted (2026-09-22) — design; implementation pending, see Consequences

## Context

Aura's integration story has no client entry point. Gravity/krystallizer booths exist and remote effectors already dial in (Phase 3), but there is no unified surface a test client, a browser, or a delivered application can connect to. PLAN Phase 8 names Prism as the WS gateway hosted as an Aura-resident component ("client connections pin here, not on Gravity; turn delivery = realm events"), and ADR-0015's node-approval endpoints were explicitly deferred until "Prism's auth exists".

The design must serve more than chat. Fluxen applications (see fluxora's Envelope model — `receiver: Vec<Session>` wrapping a `sender`/`created`/`content` message) are a pattern for arbitrary applications: chat, CMS, dashboards, commerce, freely composed. The connection plane therefore cannot bake in chat assumptions such as "every connection belongs to a user".

Constraints carried over from earlier rulings:

- ADR-0015: node identity is an ed25519 key per effector node; approval is by public key through four curl endpoints that "later hang under the account (Prism auth)".
- okm key discipline: `Key` fields reject `String` (fixed-width only); open-ended names are runtime DATA resolved through registry tables (`#[kv_index(by_name)]`), never hand-rolled byte keys.
- The effector holds no storage and receives no data-plane credentials (2026-09-20 withdrawal) — Prism is an engine-bearing host (the Aura node itself) and may hold the account registry.
- Long-polling fallback was rejected as an architecture principle; the transport contract is WS-only.

## Decision

### 1. Prism is the connection plane, hosted as an Aura-resident component

A new connection-plane crate in the aura repo owns: WS accept/upgrade, route dispatch (`/effector/<alias>`, `/admin/...`, application routes, `/assets/...`), the identity model, and the event protocol codec. The protocol definition stays owned by the Prism side of the contract (per PLAN Phase 8), but the connection plane is Aura code and rides realm events for turn delivery. Gravity, krystallizer and delivered fluxen applications are booths reached *through* Prism, not wired into it.

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

### 6. Remote effector entry at `/effector/<alias>`

The existing effector dial-in gateway (register / Call / Result / Host frames) is mounted under `/effector/<alias>`. Semantics unchanged: effector dials out, realm keys `probes` by alias, residency identity travels as `session = "<booth_type>/<key>"`.

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
- Implementation sequence: `crates/prism` (WS, identity, sets, codec) → `/admin` + upload endpoint + node-approval folding → `/effector/<alias>` mount → user registry table → `/assets/` + envelope-shaped view events.

## Update — 2026-10-29: the `init` answer's source, and the bundle's mount shape

Phase 1.10's open question is closed as a deployment fact, not as a new mechanism.

**Two legal sources, one path.** Both deliver the layout to the asking connection as ordinary `create` frames, so the client's render path never branches on where they came from.

- `file:<path>` — read at boot (YAML or JSON; YAML is a superset, so one parser serves both). The walkthrough's two-column shell (left graph, right stream) is the shipped default document.
- `event:<type>/<handler>` — submitted to the realm on the booth type's SINGLETON instance: a GLOBAL booth, one instance for the whole venue, because a layout is not per-client state.

**`webhook:<url>` is refused.** Fetching JSON over HTTP and relaying it is the previous generation's (Fluxora) shape: a network fetch on the client's first event, a second deployment dependency, and an operator-visible config surface — traded for a generality the two forms above already cover (static document, computed document). A handler that needs the URL makes the call inside its own body, where the capability belongs.

**Absence is declared, never defaulted.** With no `PRISM_INIT`, `init` answers the standard error VALUE naming the config, every other face of the plane serves normally, and the boot line states which source is live.

**Bundle mount.** The bundle IS the origin: `/` answers its `index.html`, every other unreserved path is that file inside it, and the reserved prefixes (`/ws`, `/channel`, `/code/…`, `/admin/…`, `/effector/<alias>`) are routed first so a bundle can never shadow a verb. This is stage's shape, and it is what lets the artifact work exactly as trunk emitted it — no `--public-url`, no link rewriting, one build serving both reflux (page root) and prism. `/assets/…` inside that tree is the bundle's OWN directory (`main.css`, the module media), not the mount point.

**The renderer's leg is `/channel`**, the name the renderer's page already connects to (`mount()`'s default, stage's endpoint): the host adapts to the renderer, so the bundle needs no configuration and prism ships no page of its own just to name a path.

**The layout is pushed on connect — there is no `init` verb.** A renderer's socket opens and the venue states what it has: with `PRISM_INIT=file:<path>` the boot-read frames go out at once (then an empty `init.result` terminator); with `PRISM_INIT=event:<name>` the venue EMITS that event (default name `ui_init`) and stops — whatever the application does about it (a call, a query, a whole conversation with a model) ends in view events, and those are what the renderer draws. The venue never calls the application for its layout and never waits for an answer: a call would put the venue in the middle of the application's work, and a client-asked `init` would make every host's page learn a verb the venue could have spoken itself. No source declared is legal and answers `init.error`.

**The venue is also a subscriber.** It registers one booth type for itself (`renderer`) with one handler (`view`); an application that emits `view` with `{device?: n, frames: <frame or list>}` has those frames pushed to the named renderer (or to all, when it names nobody). This is the whole forwarding path — the venue does not interpret frames, which is why a layout, an append and a patch travel identically. The boot log names the subscription, because a venue that forwards nothing looks exactly like a venue with nothing to say.

**Legs, not one shared socket.** `/ui` is the renderer's leg and speaks that wire; `/ws` stays the programmatic leg (CLI, peers, tests) with `{ev, args}`. The precedent is stage, which exposes `/channel` for the UI and `/cli` for peers: two client classes, two vocabularies, one shape per leg. Two shapes on ONE leg would put the split into every frame's parser — and the peer leg's tests, the CLI and the effector dial-in all read `args`.

**Streaming output format is a call argument.** The turn producer decides what its stream carries: gravity's turn call takes `protocol` (`data` — raw deltas, the default — or `fluxen` — view frames via `append`/`set` against the slot the layout declares). Application knowledge (which slot, what accumulates) stays with the booth; the renderer stays generic and no slot name is compiled into a bundle. Recorded on the fluxen side as `docs/decisions/0021-external-integration.md` and `docs/external-integration.md`.

## Update — 2026-10-29 (later the same day): the envelope is the host's, so the venue speaks in operations

fluxen removed the wire envelope (fluxen `460d867`, ADR 0021 decision 6). What crosses the
renderer's socket is now THE ACTION LIST — `Vec<Content<Accrete>>`, a JSON/CBOR array, always
(one action is a one-element array) — and a map is REFUSED at the renderer's boundary by name
(`frame is an envelope, not an action list`). Three things in this record therefore stop being
true, and the paragraphs above that state them are superseded, not merely amended:

- **Downstream is not `content::Message{ev, sender, created, content}`.** The envelope moved to
  the host, and prism does not need one: its own speech is expressed in the SAME vocabulary as
  the application's view, as `inject` operations (fluxen ADR 0011 — a remote write into the
  page's vals plane that is indistinguishable from an in-page emit). Two slots carry it:
  `identity` (who am I: `{device, user}`, on connect / `signup.result` / `login.result` /
  `logout.result`) and `notice` (refusals, failures, `turn.done`). A layout binds them with
  `kind: local` like any other event, and the venue never teaches a page a verb.
- **There are no `ev != "draw"` frames.** One frame shape survives: an array of operations. A
  venue that wanted to say "connected" now says it as an operation or says nothing.
- **There is no `init.result` terminator.** A message IS an action list, so the arrival of the
  list is the end of it. The empty ack existed only because an envelope could be empty.
- Unchanged: the layout is pushed on connect, `PRISM_INIT` still has the two legal sources, the
  venue is still a subscriber (`renderer`/`view`), the bundle is still the origin, and the turn
  still rides this leg (`protocol=fluxen`, slots declared here). `gravity`'s view frames arrive
  as a batch and are forwarded AS that batch, because order inside a round is semantics: the
  `set` of a text region must precede the patch that fills it.

Consequence for tests: this leg's assertions read arrays (`frame[0]["action"]`), never
`frame["ev"]`. `crates/prism/tests/ui_leg.rs` locks the new shapes, including that a booth's
answer must BE an operation to be drawn.

## Update — 2026-10-29 (later still): both directions are `view:<name>`, and a booth never answers into the page

The renderer leg is a TRANSPORT in both directions, and the marker is what makes that safe. What
follows is the settled shape; it replaces the "booth's answer becomes frames" reading that this
record carried earlier the same day.

**One vocabulary, marked.** Everything that crosses this leg is named `view:<name>`. The marker
is not decoration: a page may only ever address, and be addressed by, `view:`-prefixed names, so
it cannot forge an internal event no matter what it sends. prism subscribes to the PREFIX
(`view:*` — aura's wildcard stores the prefix, so the declaration carries the star), one
singleton route, not a list of event names it would have to know in advance.

**Uplink.** A page emits `Outflow {slot, id?, data}` (`slot` is where the interaction goes; the
word `event` is not on this wire — fluxen ADR 0023). The venue wraps it and emits
`view:<slot>` — payload `{channel_id, sender, data}` — and aura routes it to the booth bound to
that name. The chunk of knowledge the venue adds is the ENVELOPE: which channel, who is talking.
The booth learns what happened from its own declaration (`@on("view:add-to-cart")`), never from
a runtime event name.

**Downlink.** A booth emits `view:<name>` with `{name, channel_id, sender, data}`; the `name`
carries the marker too, because a Rust booth body is not told which event fired (the dispatcher
consumes the concrete name). The venue strips the marker, excludes the `sender` from the fan-out —
a page does not hear its own input echoed — and hands the payload on.

**Two payload shapes, both legal.** An OPERATION LIST is drawn. Anything else is DATA and goes
out as `inject {slot: <name>, name: <name>, data}`: the surface is the vals plane, exactly as if
the page had emitted that name itself. The venue does not pre-validate beyond decoding — a host
that checked would be enforcing a second vocabulary on top of fluxen's, and fluxen is where a
frame's validity is decided.

**What this deletes.** A booth's RETURN VALUE is not a way to reach the page. `invoke` is not the
channel here: an answer that is not an operation list draws nothing, and the venue does not
invent a name for it (`<event>.result` was such an invention; gone). What remains at the call
site is one diagnostic line, because an answer that draws nothing is otherwise hunted for in the
renderer.

**Settled the same day (the ruling was "退场"):** the venue is an emitter like any other. It
says `view:identity` (`{device, user}`), `view:notice` (`{detail}`), `view:turn.done`,
`view:turn.error`, and its own init layout rides the same plane (`view:layout`, payload = the
action list) — ordering between facts is then the queue's, not two writers' racing. The
`identity`/`notice` SLOT constants are gone; those two names survive only as conventions a layout
may bind. Facts lose their per-verb sub-name (`connected`, `signup.result`): a page that binds
`identity` always sees who it is, which is what it actually wanted.

**The uplink is an emit, and the page never sends a marked name.** The renderer's wire stays
`Outflow {dest, id?, data}` — a bare `dest`, because different consumers of a renderer make
different sense of a name and none of them should have to learn this venue's marker. prism puts it
on the plane as `view:<dest>` with `{name, channel_id, sender, args}`; the sender is filled from
the socket's own identity, so a page still has no field with which to claim another.

That needed one framework rule, which landed with it: **a declaration may name the body's entry
point separately from the event it subscribes to** (`aura_booth::ReceiveDecl.entry`, declared as
`receives: {view:add-to-cart: {entry: add_to_cart}}`). Without it, the event name IS the handler
name, and a marked event asks a script for a function called `view:add_to_cart` — unnameable — and
the delivery is dropped with nothing logged. A booth that declares the marked subscription and
keeps its plain name is the whole cost of the marker.

**A booth never answers into the page** — which is why the venue's test booths now EMIT their
answer rather than return it. A delivery discards a return value; the page is reached by an event.

## Update — 2026-10-31: the marker is `ui:`

The UI-facing marker was `view:` and is now `ui:`. Nothing about the RULE changed: an event that
crosses to a renderer is still marked in both directions, the page still sends a bare `dest`, and
the venue still subscribes to the prefix (`ui:*`, wildcard) and strips the marker on the way down.
The word changed because `view` had been carrying three meanings in this stack already —
krystallizer's session projection (`mem_core::view`), fluxen's `forcegraph::View`, and this
marker — while the marker's own job is narrower than any of them: **this event has a user
interface on the other end.** Everything written before this date that spells `view:<name>` — this
ADR's body, both PLANs, gravity's ADRs — reads `ui:<name>`.

## Update — 2026-10-31: the lifecycle's other three facts

The connection's life was ruled a stream of ordinary events (MODELING: `ui:connect`, `ui:login`,
`ui:logout`, `ui:disconnect`), and only `connect` was ever said. The other three are said now, at
the moments they happen and by the same `say` path — so each one is an envelope on the plane that
the prefix route carries back down into the slot named after it, and a layout that binds `login`
or `logout` learns the fact without asking:

- `ui:login {device, user}` on signup and on login. `ui:identity` is still said beside it, because
  the token is a thing only it can carry (the rotated credential the page must keep); `login` is
  the lifecycle fact an application binds.
- `ui:logout {device}` when the binding ends.
- `ui:disconnect {device}` when the connection goes away, said BEFORE the connection table forgets
  the device — the page's own socket is already gone at that moment, which is why this is a fact
  for applications and not a notice the venue puts on the wire for a browser.

`tests/ui_leg.rs` holds all three: the gate test reads `login` after signing up and `logout` after
logging out, and `a_disconnect_is_a_fact_for_the_application` registers a probe, opens a renderer,
drops it, and asserts the plane carried the device.

## Update — 2026-10-31: one clock, and the fact that carries the credential

Three corrections to §2/§3 above, all of them the same question asked three ways: where does a fact
live?

**The token has no expiry.** §2 described a token slid on use, with a TTL. The window is now the
DEVICE's: `Activity` (ns 54, one `at` per device) is written on the message path and read at connect
and at every uplink. A device silent past `PRISM_IDLE_WINDOW` (default 30 days) is not authenticated
any more, and the token — which is still alive — buys nothing. Two clocks for one question is two
answers waiting to disagree, so `Token.expires_at` and `Device.last_seen` are gone, and `Activity`
is the only one.

`Activity` is a table of its own rather than a field on the row it describes because the message path
would otherwise be a read-modify-write of the device row per message, racing the alias and the
binding. The device row keeps what changes rarely; the clock keeps what changes constantly.

**The device survives its own timeout.** `resolve_token` names the device; whether that name carries
permission is `idle(device)`'s answer. A silent device therefore connects as anonymous while keeping
its id and alias, is told why (`notice` with `reason: idle`), and re-earns the trust with a login on
the same connection. The auth verbs are not gated by the window — refusing the message that
re-earns the trust leaves a page with no way back. A socket that crosses the window stays up: it
loses the uplinks, not the connection, which is why `disconnect` needs no bookkeeping.

**`identity` becomes `token`, and nothing carries the user.** §4's `identity {device, user, token}`
said three things to three different consumers. The user belongs to the venue — it resolves the
device at every uplink, and the connection's auth field is the only live copy (§3) — so a page told
the user is a second home for a fact that already has one, with no computation that needs it. What
the page genuinely cannot derive is the credential: it must present the token on its next connect,
and a login rotates it, killing the old one. So the fact is `token {device, token}`, said when
minted or rotated, and the user leaves this wire entirely.

Two tests hold it on the wire: `a_silent_connection_crosses_the_window_and_a_login_reopens_it`
(short window, refused uplink, notice with its reason, login on the same device) and the gate test,
which now reads `token`/`login` BY NAME rather than in arrival order — the plane promises none.

The tables changed shape (`Device.last_seen` and `Token.expires_at` removed, ns 54 added), and rows
written before this do not decode. prism's instance is deployed nowhere with accounts worth keeping,
so there is no migration: the store is rebuilt.
