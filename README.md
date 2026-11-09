# prism — the connection plane

prism is the stack's single entry point for clients: one accept loop, one event protocol, the venue's identity, and the mounted effector surface. Design: `docs/adr/0017-prism-connection-plane.md` (en + `.zh-CN`); the model — participants, the marked wire in both directions, addressing, where state lives — is `docs/MODELING.md` (+ `.zh-CN`). It builds as a workspace sibling of aura and effector (path deps), so those checkouts must exist.

## Surfaces

| path | kind | wire | who |
|:--|:--|:--|:--|
| `/ws` | WS | `{ev, args}` in both directions | a program: the CLI, a peer, a test |
| `/channel` | WS | downstream an ACTION LIST (`Vec<Content>`), upstream `content::Outflow` | a renderer page |
| `/effector/<alias>` | WS | effector-protocol frames: `Challenge{nonce}` → signed `Register` → `Registered` / `Refused{reason}` | a node dialling in |
| `/code/<sha256>` | HTTP | the bytes registered under that hash, immutable | anyone holding the hash |
| `/admin/nodes`, `/admin/nodes/{alias}/approve`, `/admin/revoke`, `/admin/dead` | HTTP | JSON | the operator |
| `/app.css`, then everything else | HTTP | the renderer bundle, plain GETs only | a browser |

There is no second RPC surface: every face rides the same accept loop, and `handle_conn` reads the request head RAW and branches — the specific `/admin/…` paths first, then `/code/…`, the bundle, and `/effector/<alias>` — replaying the head into the WS upgrade only for the requests that are sockets. A new HTTP face goes into that routing queue; it cannot be written inside the tungstenite handshake callback, which can only decorate a successful upgrade.

## Two legs, one wire each

The PROGRAMMATIC leg (`/ws`) answers a connect with `{ev: "connected", args: {device_id, user_id, token}}` — both ids as text, `user_id` null for an anonymous connection — and then follows one rule: the client's `ev` names a booth TYPE, the handler name equals that name, and the answer comes back as `<ev>.result`. Failures are `error` VALUES with the socket staying up; a socket is never closed to report a business failure. `?protocol=json` (or `?codec=`) selects the debug codec; absent means CBOR, fixed for the connection.

The RENDERER's leg (`/channel`) speaks fluxen's wire and therefore has no verbs: downstream is always an ARRAY of operations, and upstream is `content::Outflow {dest, id?, data}` where `dest` is a bare application name. The venue's own speech is `inject`s into the slots a layout declares — `token` (`{device, token}`, the credential the page keeps), `connect` (`{device}`, the fact an application answers with its layout), `login`, `logout`, `disconnect` — plus `notice` (`{detail}`) for refusals and ends. Everything UI-facing is `ui:<name>` on the plane and carries no marker on the page's wire: the venue adds it on the way in and strips it on the way out, which is why a page's `dest` and the frames it reads are bare.

## Identity

The venue knows DEVICES and USERS, both verified, and a token is the only credential.

| collection | ns | owner | key | payload |
|:--|:--|:--|:--|:--|
| `Account` | 50 | the auth booth | `Uuid7` | `username`, `nickname` |
| `Device` | 51 | the venue | `Uuid7` | `user`, `alias`, `first_seen`, `revoked` |
| `Node` | 52 | the venue | proxy id | `alias`, `public_key`, `status`, `created_at` |
| `Token` | 53 | the venue | `Uuid4` | `device`, `user`, `issued_at`, `revoked_at` |
| `Activity` | 54 | the venue | `Uuid7` | `at` — the ONE clock |
| `Credential` | 55 | the auth booth | `Uuid7` (= user id) | `hash` |

`?token=` is the only restore path — a device id names a device and authenticates nothing. Freshness is the device's own clock: `Activity` is written on the message path (throttled, with one last stamp when a connection ends) and read at connect and at every uplink, so a device silent past `PRISM_IDLE_WINDOW` connects ANONYMOUS while keeping its id and alias, and a login on the same connection restores the usual name. Local verbs are the account's: `login`, `signup` and `logout`, plus `devices` (the account's own device list) and `alias` (this device's name) — no booth can answer either, since a booth cannot enumerate devices — beside `broadcast` (the fan-out helper) and `ping` (liveness); every other name belongs to a booth.

AUTHENTICATION IS THE VENUE'S, THE PROOF IS A BOOTH'S. The verbs, the device↔user binding and the token live here and are not configurable; the credential proof and the login surface are booth `auth`'s, which answers ONE call — `verify` or `register`, returning the user id a credential names — after which prism binds the device and rotates the token. A password is never an emit: every emit is persisted as an MQ row and an unroutable payload is served by `/admin/dead`, so the ask is a call. A gate is declared on the RECEIVE (`on_as(…).gated()` in Rust, `@on(…, auth=True)` in python, the options hash in steel), registration carries it onto the route, and prism asks one question of that table for both legs and both addressing modes. A name matching no route is not gated (fail-open). A gated event refused for an anonymous connection says `ui:auth.required {event}` to that device beside the `notice` that tells the person, and that is what opens the login surface.

## Applications are booths

The venue forwards; it does not interpret. An application emits `ui:<name>` with `{name, device | user, sender, data}`: `name` states where it goes, `device` addresses one connection while `user` addresses an account and the venue fans it out to every live device bound to it (a booth cannot enumerate devices), `sender` is excluded, and the payload goes out as one batch per connection — an operation list is DRAWN, anything else is DATA and rides an `inject` into the slot named by the event. THE LAYOUT IS THE APPLICATION'S ANSWER TO CONNECT: there is no `init` verb, no `PRISM_INIT` and no layout file here; a booth that declares `ui:connect` draws the page, and a venue with no such booth draws nothing and stays up.

No turn verb lives here. A conversation is not the venue's key: the address is a device and the authentication a user, while the conversation is the application's own data. gravity's booth declares `ui:turn.submit` itself, keyed on the envelope's `device`, and says its frames through the same `ui:` plane.

## Nodes and mounts

`/effector/<alias>` is the dial-in mount, and the handshake is HOST-FIRST and posture-blind on the wire: the venue sends the nonce, the node answers a signed `Register`, and the answer is `Registered` or `Refused{reason}` before the close. `PRISM_IDENTITY=required` gates on `NodeStore::verdict` (an unapproved or conflicting key is refused with its reason); `open` means no registry, never no proof — the signature is verified in both postures. A node's identity is `(alias, public_key)`, and the handshake never learns a user.

`PRISM_MOUNTS` is a JSON manifest of mounts, replayed at every boot because `Remote` booth definitions are not persisted: each entry is `{type, alias, carrier, language, runner, encoding?}`, where `alias` must equal the dial-in path and `runner` is a FIXED deployment artifact. A skill travels as a call argument, so a new skill — or a new version of one — mints no type, no ns and no route. Absent `PRISM_MOUNTS` means no nodes.

## Configuration

| variable | meaning | absent |
|:--|:--|:--|
| `PRISM_IDENTITY` | `required` \| `open` — the trust posture | boot error; the posture is never defaulted |
| `PRISM_ADDR` | listen address | `127.0.0.1:8765` |
| `PRISM_DATA` | the registry directory | `./prism-data` |
| `PRISM_ASSETS` | the renderer bundle root — the bundle IS THE ORIGIN | nothing served at `/`; the boot log says so |
| `PRISM_MOUNTS` | the node mount manifest | no mounts |
| `PRISM_IDLE_WINDOW` | device idle window, seconds | 30 days |

## Run

```sh
PRISM_IDENTITY=open cargo run
PRISM_IDENTITY=open PRISM_ADDR=127.0.0.1:9000 PRISM_DATA=/tmp/venue cargo run
```

The binary registers the shipped auth booth, the echo booths (the carrier demo: one per embedded language, plus the sender and the gated one), and the declared mounts, then states which faces are live. A venue with no bundle still serves identity and the peer leg; it just has no page.

## Test

```sh
cargo test -p prism --lib --test admin_nodes --test auth_seam --test code_export \
  --test echo_e2e --test effector_handshake --test effector_mount --test identity_e2e \
  --test peer_leg --test runner_mount --test turn_invoke_skill --test ui_leg -- --test-threads=1
```

71 tests across 12 binaries (measured 2026-11-09). The list is enumerated on purpose: `cargo test --workspace` stops at the first failing binary, so one red or hung binary hides everything after it. Every file boots a real engine and a real socket — there is no mocked transport — and `--test-threads=1` is the only mode the suite runs in, which makes a missing deadline look like patience: a read the leg legitimately never sends blocks forever.

`turn_invoke_skill` drives a real turn, so gravity's guests must be prebuilt first (`cargo build --bins -p gravity-booth` in `~/world/gravity`); prism's cargo does not rebuild a sibling's binaries, and a stale guest surfaces as a HANG. The renderer's leg is checked the same way, with a JSON-pinned WS client on `/channel?codec=json`: a message there IS an array of operations, so a test asserts `ops[0]["action"]` and never `frame["ev"]`.

The wire types and both codecs live in `crates/prism-protocol`: one model, two encodings, one round-trip test per shape.