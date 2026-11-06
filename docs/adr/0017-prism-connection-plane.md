# 0017 — Prism connection plane: dual identity, the connection set, and one shape per leg

> **Languages:** [English](0017-prism-connection-plane.md) (primary) · [中文](0017-prism-connection-plane.zh-CN.md)

**Status:** Accepted (2026-09-22) — implemented in `crates/prism`; the renderer leg's wire is locked
by `crates/prism/tests/ui_leg.rs`, the peer leg's by `crates/prism/tests/peer_leg.rs`.

## Context

Aura's integration story has no client entry point. Gravity/krystallizer booths exist and remote
effectors already dial in (Phase 3), but there is no unified surface a test client, a browser, or a
delivered application can connect to. PLAN Phase 8 names Prism as the WS gateway hosted as an
Aura-resident component ("client connections pin here, not on Gravity; turn delivery = realm
events"), and ADR-0015's node-approval endpoints were explicitly deferred until "Prism's auth
exists".

The design must serve more than chat. A fluxen application is a page the application draws by
emitting an action list (`Vec<Content<Accrete>>`, fluxen ADR-0011/0021), with its own slots and its
own events behind it; chat, CMS, dashboards and commerce are all that shape, freely composed. The
connection plane therefore cannot bake in chat assumptions such as "every connection belongs to a
user".

Constraints carried over from earlier rulings:

- ADR-0015: node identity is an ed25519 key per effector node; approval is by public key through four
  curl endpoints that "later hang under the account (Prism auth)".
- okm key discipline: `Key` fields reject `String` (fixed-width only); open-ended names are runtime
  DATA resolved through registry tables (`#[kv_index(by_name)]`), never hand-rolled byte keys.
- The effector holds no storage and receives no data-plane credentials (2026-09-20 withdrawal) —
  Prism is an engine-bearing host (the Aura node itself) and may hold the account registry.
- Long-polling fallback was rejected as an architecture principle; the transport contract is
  WS-only.

## Decision

### 1. Prism is the connection plane, hosted as an Aura-resident component

The connection plane is this repository's `crates/prism`, with the wire codec split into
`crates/prism-protocol`. It boots an aura engine and serves every client-facing face on ONE accept
loop: WS upgrade on `/ws` (programs) and `/channel` (renderers), `/admin/...`, `/effector/<alias>`,
the `/code/<sha256>` export (ADR-0027), the venue's own `/app.css`, and the renderer bundle as the
fallthrough. Protocol definitions stay Prism's side of the contract (PLAN Phase 8); the plane is
Aura code and rides realm events for turn delivery. Gravity, krystallizer and delivered fluxen
applications are booths reached *through* Prism, not wired into it.

### 2. Dual identity: device and account, one credential, one clock

The plane recognises two identities and one credential:

- **Device** — minted at the first connect (`Uuid7`), the address of a connection, anonymous until a
  login binds it. Dashboards, browsing, add-to-cart and chat itself all work with no account.
- **Account** — `signup` / `login` bind the device to a `user_id` (`Uuid7`). The device row keeps the
  binding, so a reconnect comes back bound without re-authenticating.
- **Token** — the credential (`Uuid4`), presented on the handshake as `?token=`, rotated on signup,
  login and logout. It names a device; an id the client can guess (`?device=`) authenticates
  nothing.

Freshness is the DEVICE's, not the token's. `Activity` (ns 54, one `at` per device) is written on the
message path and read at connect and at every uplink; `Token` carries no expiry and `Device` no
`last_seen`. A device silent past `PRISM_IDLE_WINDOW` (seconds; default 30 days) is no longer
authenticated, and the token — still alive — buys nothing. Two clocks for one question is two answers
waiting to disagree, so there is exactly one.

`Activity` is a table of its own rather than a field on the row it describes, because the message
path would otherwise be a read-modify-write of the device row per message, racing the alias and the
binding. The device row keeps what changes rarely; the clock keeps what changes constantly.

A device survives its own timeout. `resolve_token` names the device; whether that name carries
permission is `idle(device)`'s answer. A silent device therefore connects as anonymous while keeping
its id and alias, is told why (`notice` with `reason: idle`), and re-earns trust with a login on the
same connection. The auth verbs (`login`, `signup`, `logout`) are not gated by the window — refusing
the message that re-earns the trust would leave a page with no way back. A socket that crosses the
window stays up: it loses the uplinks, not the connection.

Nothing evicts a connection for being anonymous or quiet. Public services legitimately live on
anonymous connections, and what the window takes away is authentication, never the socket.

Whether an event requires an account is a business decision. A booth type declares it in its
persisted `interface_schema`, and the venue enforces that declaration before delivery, resolving the
event through the router: every booth type routed for the event is asked, and a type declaring any
`auth` block gates its events. The framework only makes "is the sender a device or a user" queryable,
and answers a required-auth event from an anonymous sender with a standard error value — the socket
stays up. Commerce declares it on "place order"; chat may never.

**Authentication is the venue's; PROVING a credential is a booth's.** The shape is SSO inside the
platform, with prism as the identity provider. The verbs (`login`, `signup`, `logout`), the
device↔user binding, the token and the clock are prism's and are not configurable: an application
never authenticates anybody, never sees a password, a verifier or a token, and never holds a login
flow — it declares that an event needs a user and receives `sender {device, user}`. What a deployment
MAY replace is the proof and the surface: a booth of type `auth` answers ONE call (`verify` /
`register`) with the user id a credential names, and the venue then binds the device and rotates the
token. The seam is a CALL and never an event, because an emit is persisted as an MQ row (aura,
`realm/src/events.rs`) and a payload that reaches no route is served by `/admin/dead` over plain
HTTP (§7) — a password must not travel that way. The shipped booth keeps its credentials in this
instance (§9) and a deployment that wants an external IdP, an LDAP bind or a redirect flow replaces
that policy; the venue does not move. A venue with no `auth` booth is legal: it serves the anonymous
plane and every login attempt answers the standard error naming the missing type. (OAuth2's redirect
leg — leaving the page and coming back — is NOT built: the renderer has no "leave this page"
operation and the venue reserves no callback prefix.)

### 3. One connection set; auth state is a per-connection field

The gateway holds ONE set of connections. Each connection carries its own auth state as a field:
anonymous (the nil uuid) until a successful `login` or a live token fills it. Group fan-out — to all,
to authenticated-only, or to a declared subset — iterates the single set and selects by that field:
the selection is a predicate evaluated once per fan-out, not a second collection kept in sync. Two
collections would store the same fact twice, and every state change (login, logout, revocation)
would have to write both or the sets would drift; one set with a per-connection field makes the drift
unrepresentable, and the fan-out traversal the two sets saved is not a hot path at connection scale.

The renderer's connections are a table of their own — different writers, different frame types — but
they carry the SAME identity facts: a device, and an auth field a `login` writes. Keeping the tables
apart is what lets one leg speak Accrete without teaching the other about it.

Revocation is the case that proves the field is enough. `POST /admin/revoke {device_id}` clears the
binding and stamps the device in the registry, clears the auth field on BOTH legs, sends `revoked` to
a peer connection before closing it, and closes a renderer connection whose sender it drops — a
revoked device's page does not keep watching a session that no longer exists.

### 4. Two legs, one shape each: `/ws` and `/channel`

The plane exposes two client legs. No path carries two shapes.

**`/ws`, the programmatic leg** (CLI, peers, tests): `{ev, args}` in both directions. The protocol
does not encode direction — the shape that goes out is the shape that comes in, and dispatch cannot
branch on it. A bare `ev` is a CALL to the booth type of that name, answered `<ev>.result` — except
the venue's own verbs (`signup`, `login`, `logout`, `broadcast`, `ping`), which it answers itself in
that same shape. An `ev` beginning `ui:` is an EMIT onto the plane, and its delivery comes back as
`{ev: "<name without the marker>", args}`, because a program reads data rather than drawings.

**`/channel`, the renderer's leg**: there are no verbs on it. Downstream is THE ACTION LIST —
`Vec<Content<Accrete>>` as a JSON/CBOR array, always an array (one action is a one-element array). A
map is refused at the renderer's boundary by name (`frame is an envelope, not an action list`): the
envelope belongs to the host, and this is the host. Upstream is `Outflow {dest, id?, data}` —
fluxen's own uplink; the venue reads `dest` and `data`.

Two shapes on one leg would put the split into every frame's parser, and the peer leg's tests, the
CLI and the effector dial-in all read `args`.

Both legs answer a connect: `/ws` with `{ev: "connected", args: {device_id, user_id, token}}`, and
`/channel` with the facts of §6 — a renderer's page needs no handshake verb of its own.

Two encodings serve both: CBOR (the default) and JSON (debugging), selected at handshake
(`?protocol=json`, with `?codec=json` honoured as the same request) and fixed for the lifetime of the
connection.

The persistent WS stays connected. `login` rides it on both legs — `{ev: "login", args}` on `/ws`, an
`Outflow` to `login` on `/channel` — never a separate HTTP round trip.

### 5. The `ui:` marker: one vocabulary for the renderer leg

Everything that crosses to a renderer is marked in both directions: `ui:<name>`. The marker is not
decoration — a page may only ever address, and be addressed by, marked names, so it cannot forge an
internal event whatever it sends. The word is `ui` because the marker's job is narrow: this event has
a user interface on the other end.

**Downlink.** An application emits `ui:<name>` with `{name, device?, sender?, data}`. `name` carries
the marker too, because a Rust booth body is not told which event fired (the dispatcher consumes the
concrete name). The venue strips the marker, excludes the `sender` from the fan-out — a page does not
hear its own input echoed — and forwards the payload.

**Two payload shapes, both legal.** An operation list is DRAWN. Anything else is DATA and goes out as
`inject {slot: <name>, name: <name>, data}` — the surface is the vals plane, exactly as if the page
had emitted that name itself. The venue does not pre-validate beyond decoding: a host that checked
would be enforcing a second vocabulary on top of fluxen's, and fluxen is where a frame's validity is
decided. A frame that is not an Accrete operation is named and skipped, so one typo still draws the
rest. The same data reaches the program leg as `{ev, args}`, in the peer connections the event
addresses — one emit, two renderings.

**Uplink.** The page sends a bare `dest`; the venue puts the marker on it and emits `ui:<dest>` with
`{name, device, sender: {device, user|null}, data}`. The sender is this socket's own identity, looked
up once: a page has no field with which to claim another. The marker belongs to the venue on this
side because different consumers of a renderer make different sense of a bare name, and none of them
should have to learn this venue's spelling. A program on `/ws` writes the marker itself; there is no
venue in between to add it.

**Addressing.** The address is a device, or the absence of one for every renderer. A channel is an
application's business, not a venue field (ADR-0018), so no payload on this wire carries one: "send
to channel 7" is a sentence only the booth that owns channel 7 can say, and the booth resolves a
group into a recipient list.

**One route.** The venue subscribes to the prefix — `ui:*`, a Singleton route. The declaration
carries the star, because aura decides "wildcard" by a trailing `*`; the venue therefore holds one
subscription instead of a list of names it would have to know in advance.

**Declarations name the entry.** A marked name is not a handler name, so a declaration names the
body's entry point separately (`aura_booth::ReceiveDecl.entry`, written
`receives: {ui:add-to-cart: {entry: add_to_cart}}`). Without it the event name IS the handler name, a
script would be asked for a function called `ui:add_to_cart` — unnameable — and the delivery would be
dropped. A booth that declares the marked subscription and keeps its plain name is the whole cost of
the marker.

**Identity rides the payload.** A handler receives `{name, device, sender: {device, user|null},
data}`. `Ctx`, `Job` and `InstanceId` carry no identity fields, and identity is deliberately not the
storage partition key either: a partition may be keyed by any business dimension (partitioning.md
§2.1) — the address answers "who serially processes", the envelope answers "who initiated".

**A booth never answers into the page.** A delivery discards a return value; a booth that wants the
page to see something EMITS `ui:<name>`. An answer that is not an operation list draws nothing, and
the venue invents no `<event>.result` for it — what remains at the call site is one diagnostic line,
because an answer that draws nothing is otherwise hunted for in the renderer.

### 6. The venue is an emitter: facts about a connection

The venue speaks the same vocabulary as everyone else. A fact it wants a page to hold is
`ui:<fact>`, and the venue registers one booth type for itself to carry it (`renderer`: a Rust body
in this process, one singleton instance, subscribed to `ui:*`). The fact lands in the vals slot named
after it, which a layout binds with `kind: local` like any other event. There is no second vocabulary
and no per-verb name for a page to learn.

- `ui:token {device, token}` — the credential, said at connect and again whenever it is minted or
  rotated (signup, login, logout). It carries no user: the user is the venue's own answer at every
  uplink, and a page told a user would be a second home for a fact that already has one. The token is
  what the page cannot derive and must keep.
- `ui:connect {device}` — a connection opened, said on `/channel`. Whoever declared it answers with
  the layout; a venue with no such booth draws nothing while the connection stays up.
- `ui:login {device, user}` — a signup or a login: the binding, and the lifecycle fact an application
  binds.
- `ui:logout {device}` — the binding ended.
- `ui:disconnect {device}` — the connection went away, said BEFORE the connection table forgets the
  device. The page's own socket is gone by then, which is why this is a fact for applications and not
  a notice the venue puts on the wire for a browser.
- `ui:notice {detail}` — refusals and failures: an auth refusal names the event, a failed login
  carries the registry's message. `ui:notice {detail, reason: "idle"}` when the window has taken
  authentication away.

The turn's end is two more facts on the same plane, emitted by the application that ran it and
addressed to the device that asked: `ui:turn.done` and `ui:turn.error`. A program on `/ws` reads them
as `turn.done` / `turn.error`; a renderer draws them into the slots of those names.

The plane promises no order between two facts. A page binds both and assumes neither arrived first.

### 7. Admin prefix: node approval, revocation, and the dead ring

Under `/admin`, JSON in and JSON out on the same accept loop: the four node-approval endpoints from
ADR-0015 (`POST /nodes`, `GET /nodes`, `POST /nodes/{alias}/approve`, `DELETE /nodes/{alias}`), plus
`POST /admin/revoke {device_id}` and `GET /admin/dead` — the engine's dead ring, listing events that
reached nobody and deliveries that reached a queue but could not call the body. The node registry
lives in prism's own okm instance (ns 52) beside the identity tables.

Booth code is not delivered through this prefix. A booth is a registered type in the running process
(`Gateway::with_echoes` mounts the echoes and gravity's turn executor; `PRISM_MOUNTS` replays the
declared node mounts at boot), and `/admin` is the operator's face, not a code-delivery one.

No `/admin` route authenticates the operator: the interim rule is curl-first, and the deployment
boundary — the posture `PRISM_IDENTITY=required|open`, stated in the boot log — is the boundary. The
account folding of ADR-0015's step 3 is a decision this plane has not taken.

### 8. Remote effector entry at `/effector/<alias>`

The existing effector dial-in gateway (register / Call / Result / Host frames) is mounted under
`/effector/<alias>`. Semantics unchanged: effector dials out, realm keys `probes` by alias, residency
identity travels as `session = "<booth_type>/<key>"`. The URL segment IS the alias claim and the
`Register` frame repeats it; the two must agree or the connection is refused, because a path that
admitted any alias would make the URL a lie on every later frame.

### 9. Identity and accounts in okm: id-first access, and no verifier in the venue's own rows

Prism's own okm instance holds five tables across TWO owners. The venue's are `Device` (ns 51):
`Uuid7` key; the binding, an alias, `first_seen`, and the revocation stamp; a `by_user` index for a
"my devices" list, clock-ordered because the ids are `Uuid7`. `Token` (ns 53): `Uuid4` key — a
credential wants unpredictability, not order; device, user, `issued_at`, `revoked_at`; a `by_device`
index so rotation, logout and revocation retire the set without a scan. `Activity` (ns 54): the clock
of §2. The auth booth's are `Account` (ns 50): `Uuid7` key; username, nickname; a `by_name` index
serving exactly one hot path — login-time exact lookup. `Credential` (ns 55): `Uuid7` key = the user
id; the argon2id PHC hash and nothing else.

The last two are two tables because a verifier is not profile data: a listing, a dump or an export of
accounts must not carry something that can be cracked offline. `Registry::bind_login` is the only
place a device learns a user, and it takes the id the booth named — the venue cannot verify a
password and never holds one. A swapped booth stores its own rows wherever it likes; the venue keeps
only what it has always kept.

Keys are 16 raw bytes and `get`/`scan` hand the key back, so no row repeats its own name and there is
no key-versus-payload state for discipline to hold.

Everything except the username lookup uses ids: sessions, message senders and authorization checks
never touch a name. The nickname is value data; the framework never resolves a user by nickname. A
login revokes the device's older tokens before minting the new one, so a leaked token dies at the next
login; logout revokes the whole set, clears the binding and hands the page a fresh anonymous token;
revocation stamps the device, and only a fresh login clears the stamp.

### 10. Fluxen integration: the bundle is the origin, the layout is the application's

Static resources are served as plain downloads on the same accept loop, carrying no event semantics.
The bundle IS the origin: `PRISM_ASSETS` names the directory, `/` answers its `index.html`, and every
path no route claimed is that file inside it — so the artifact works exactly as trunk emitted it, with
no `--public-url` and no link rewriting, one build serving both reflux (page root) and prism. The
reserved prefixes (`/ws`, `/channel`, `/code/…`, `/admin/…`, `/effector/<alias>`, `/app.css`) are
routed first, so a bundle can never shadow a verb. `/assets/…` inside that tree is the bundle's OWN
directory (`main.css`, the module media), not the mount point. Hashed `.js`/`.wasm` are immutable for
a year and everything else revalidates; a directory, a `..` and a miss are the same 404. The venue's
own stylesheet is compiled in and linked into the entry document after the bundle's sheets, so the
page's layout wins by order rather than by specificity.

The layout is not a deployment fact. There is no `init` verb and no layout source to configure: on
connect the venue says `ui:connect {device}`, and whatever the application does about it — a call, a
query, a whole conversation with a model — ends in `ui:<name>` view events, which the renderer draws.
The venue never calls the application for its layout and never waits for an answer; a call would put
the venue in the middle of the application's work, and a client-asked `init` would make every host's
page learn a verb the venue could have spoken itself. A deployment with no bundle is legal: it serves
identity, turns and peers, and says so at boot.

The frames are the renderer's vocabulary, and the venue does not interpret them — which is why a
layout, an append and a patch travel identically. gravity's view frames arrive as a batch and are
forwarded AS that batch, because order inside a round is semantics: the `set` of a text region must
precede the patch that fills it.

Streaming output format is a call argument. The turn producer decides what its stream carries:
gravity's turn call takes `protocol` — `data` (raw deltas, the default) or `fluxen` (view frames via
`append`/`set` against the `slots` the same call declares) — or an `event` naming the event raw
deltas are spoken as. Application knowledge (which slot, what accumulates) stays with the booth; the
renderer stays generic and no slot name is compiled into a bundle. Recorded on the fluxen side as
`docs/decisions/0021-external-integration.md` and `docs/external-integration.md`.

## Honest semantic cost

- **Fan-out filters per connection.** With one set, every broadcast evaluates the auth predicate over
  all connections — O(all) instead of O(target). Accepted: connection counts are machine-scale small,
  and the alternative (two sets, moved on login) duplicated the auth fact and left every future
  revocation path a second collection to remember.
- **The window de-authenticates without disconnecting.** An idle page keeps a socket it cannot use
  until it logs in, and nothing caps anonymous connections at all — an anonymous flood holds
  connections indefinitely. Mitigation belongs to deployment (connection caps per IP), not to the
  protocol.
- **Device identity is a convenience anchor whose loss is bounded by business design, not by the
  framework.** A stolen device id impersonates the device's anonymous history. How much that matters
  is a per-application decision: the commerce pattern keeps the cart in localstorage and syncs/merges
  with the server only after login, so the pre-auth history's authority stays local. Account
  credentials are the real secret; applications decide what to entrust to the addressable identity.
- **Dual encoding doubles the codec surface.** Every frame type has two serialization paths; protocol
  evolution must keep CBOR and JSON shape-compatible (same field sets, different physical encodings)
  or debug clients diverge from production ones.
- **The marker costs a declaration.** Every marked subscription needs its `entry` beside it, or the
  delivery is dropped for want of a function whose name no language can spell. A booth that keeps its
  plain handler name pays that cost once per event.
- **The venue forwards frames it does not interpret.** A malformed operation is named in the log and
  skipped, and a payload that carries nothing deliverable is logged rather than rejected — the wire
  has no place to report a frame's validity except fluxen's own decoder on the page.

## Consequences

- The renderer leg's wire is locked by `crates/prism/tests/ui_leg.rs`: the two connect facts (`token`,
  `connect`), a silent connection crossing the window and a login reopening it on the same device,
  `ui:disconnect` as a fact for the application, data landing in the slot named by its event, the
  sender excluded from its own fan-out, addressing by device, the auth declaration refusing and then
  delivering, a revocation closing the socket, and `/ws` keeping `{ev, args}`.
- The turn's exit is locked end to end: `crates/prism/tests/peer_leg.rs` submits `ui:turn.submit` on
  the peer leg and reads `turn.delta` / `turn.done` back, while an anonymous submit gets the standard
  error value and the socket stays up.
- Integration tests have a real entry point: a WS client connects, logs in, drives events and observes
  realm events, and the same venue serves a browser page at `/` and a program at `/ws` without either
  learning the other's shape.
- Phase 8's "turn delivery = realm events" is the binding contract: one emit in, one emit out, the
  device as the address, the declaration gate in front of it.
- The auth declarations are the single ACL surface on both legs — the venue reads each routed type's
  persisted `interface_schema`, so there is no second table to drift — and a later chat or commerce
  application defines its own per-event declarations, the framework shipping none beyond the turn
  executor's and the test booths'.
- ADR-0018 holds on the wire: no payload carries a channel, and a booth that emits a group name as an
  address reaches no renderer.
- The auth seam is locked by `crates/prism/tests/auth_seam.rs`: a venue with NO `auth` booth answers
  login and signup with the standard error and keeps serving the anonymous plane, and a SWAPPED booth
  answers the same call — prism binds the user id it named, and the rotated token restores both the
  user and the device. The shipped booth's two tables and its one-error-for-both-halves rule are
  locked by `crates/prism/src/auth.rs`'s unit tests.
- A deployment declares three facts: `PRISM_IDENTITY` (required, `required` or `open`),
  `PRISM_ASSETS` (optional; absent is a legal install that serves identity, turns and peers), and
  `PRISM_IDLE_WINDOW` (optional, seconds, default 30 days). A store starts empty: the venue's
  `Device` (51), `Token` (53), `Activity` (54) and the shipped auth booth's `Account` (50),
  `Credential` (55) are the shape, and no earlier row shape is read.