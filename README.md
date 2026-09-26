# prism — the connection plane

WS gateway hosting Aura actors, the entry point of the stateless-agent
stack (design: `docs/adr/0017-prism-connection-plane.md`, en/zh).
Built on the aura engine as a workspace sibling (`../../aura`,
`../../probe` by path — same triangle as aura's own cross-repo deps).

## Scope of this landing (echo plane + identity, PLAN Phase 0/1 reduced)

Runs a gateway whose actor set is the echo in every embedded language —
the minimal example that proves each carrier end to end through a real
socket — plus the §2/§3/§7 identity plane (device anchors, accounts,
one connection set with a per-connection auth field). Wire rules
(ADR-0017 §4, honored here):

- ONE frame shape in BOTH directions: `{"ev": "<name>", "args": {...}}`.
  The protocol encodes no direction; dispatch cannot branch on it.
- The client's `ev` names an actor TYPE; the handler name equals the
  event name (the multi-entry model). The result returns to the CALLING
  connection as `{"ev": "<name>.result", ...}`; failures are
  `{"ev": "error", "args": {"ev", "message"}}` VALUES — the socket
  never closes for a business error.
- `{"ev": "broadcast", "args": x}` fans the frame out to every live
  connection (the connection-set traversal of §3, minus the auth sets).
- `{"ev": "ping"}` answers `ping.result` — a gateway-liveness verb, no
  actor involved.
- Handshake: `GET /ws`, `?protocol=json` selects the debug codec;
  absent = CBOR (production default), fixed for the connection.
  `?device=<id>` echoes a known device anchor (§2).
- Every connect is answered `{"ev":"connected","args":{device_id,
  user_id|null}}` — device assignment and restored binding, one frame.
- `signup` / `login` are ordinary business events (§4) on the same
  socket, verified against argon2id hashes in prism's OWN okm instance
  (§7 — accounts never enter aura's planes); login writes the
  connection's auth field (§3, one set).
- Identity reaches actors through the PAYLOAD (§7 amendment): the
  handler receives `{"sender": {"device","user"}, "args": <original>}`;
  `echo_sender` answers with the sender half as the live proof. Ctx,
  Job, InstanceId stay identity-free.
- `broadcast` accepts `"to": "all"|"auth"` (default all) — the
  predicate over the one connection set.
- Per-event auth ENFORCEMENT (§2): an actor declares `{auth:
  {"<handler>": ...}}` in its `interface_schema` — presence of the
  handler name is the rule (the value is reserved). The gateway reads
  the SAME persisted copy `ctx.interface_schema` reflects (the
  upload-time introspection is the single declaration surface; no
  second surface, no per-connection ACL table), checks it BEFORE the
  call, and answers `{"ev": "error", "args": {"ev", "message":
  "authentication required"}}` to an anonymous sender — the event
  never reaches the actor, the socket stays up. `echo_priv` is the
  live proof.

Echo actors (source: `crates/prism/src/actors.rs`; a language's carrier
absent from the build skips its echo with a boot note, never silently):

| type          | carrier  | form |
|---------------|----------|------|
| `echo_steel`  | steel    | `(define (echo_steel args) args)` |
| `echo_sender` | steel    | returns the envelope's sender half — the identity proof |
| `echo_priv`   | steel    | declares `auth` for its handler — the per-event auth proof |
| `echo_python` | python   | `def echo_python(args): return args` |
| `echo_nu`     | nushell  | `export def echo_nu [args] { $args }` |
| `echo_wasm`   | wasmtime | Rust `#![no_std]` cdylib, `examples/echo-wasm` (the full-power path, ADR-0026 §4 — pure echo needs no bridge: zero okm, zero deps) |

Deliberately NOT here (the rest of ADR-0017, planned as later phases):
logout / server-side revocation, `/admin` actor upload, `/probe/<alias>` mount
(the probe gateway currently lives in `aura/crates/engine`), `/assets`,
the CBOR-vs-JSON DevTools panel. They ride prism PLAN Phase 1+/1.8.

Landed early (PLAN Phase 1.9, ADR-0027 §4): `GET /code/{sha256}` — the
static code export rides the SAME accept loop as `/ws` (a raw head read
routes `/code/...` to a plain HTTP download, every other path replays
its head into the WS handshake). Content is a direct same-process read
of aura's meta plane; no auth — the hash is the capability, the body
re-hashes to its own URL. A malformed address is 400, an absent blob
404; `Cache-Control: public, max-age=31536000, immutable`.

Landed, step 1 (PLAN Phase 1.8, ADR-0015 §4/§5/§7): the node identity
trust plane's prism-side data surface. The boot REQUIRES a declared
posture — `PRISM_IDENTITY=required|open`, no default, stated in the
startup log — and the node registry (`{alias, public_key, status,
created_at}`) lives in prism's own okm instance with its four
curl-first approval endpoints on the same accept loop:

```sh
curl -X POST localhost:8765/admin/nodes -d '{"alias":"home-pc","public_key":"..."}'
curl localhost:8765/admin/nodes
curl -X POST localhost:8765/admin/nodes/home-pc/approve   # unique pending; key optional
curl -X DELETE localhost:8765/admin/nodes/home-pc         # tombstones (status=revoked)
```

The handshake EXECUTION (challenge frames gating a live probe
connection) lands with the `/probe/<alias>` mount — until then
`required` is the declared posture and the verdict table is in place,
test-locked, with no node yet to gate.

## Run

```sh
PRISM_IDENTITY=open cargo run          # the posture is required (ADR-0015 §7 — no default)
PRISM_IDENTITY=open PRISM_ADDR=127.0.0.1:9000 cargo run   # PRISM_DATA=<dir> = registry storage (default ./prism-data)
```

Debug client (JSON codec, stdlib only):

```sh
python3 -m pip install websockets && python3 - <<'EOF'
import asyncio, json, websockets
async def main():
    async with websockets.connect("ws://127.0.0.1:8765/ws?protocol=json") as ws:
        await ws.send(json.dumps({"ev": "echo_steel", "args": {"msg": "hi"}}))
        print(json.loads(await ws.recv()))
asyncio.run(main())
EOF
```

Rebuild the wasm echo after editing `examples/echo-wasm`:

```sh
./examples/echo-wasm/build.sh   # writes echo_wasm.b64, embedded by include_str!
```

## Test

```sh
cargo test --workspace
```

`crates/prism/tests/echo_e2e.rs` boots engine + gateway and drives a
real WebSocket: every language round-trips through BOTH codecs, the
broadcast fan-out reaches two connections, unknown types and garbage
frames return error VALUES without closing. `identity_e2e.rs` covers
§2/§3/§7 end to end: device assignment, signup/login, the binding
across reconnects via `?device=`, the auth predicate over the one
connection set, and the sender envelope on the wire. The codec pair
itself (dual-encoding maintenance contract: one model, two codecs, one
round-trip over every shape) is locked in `crates/prism-protocol`.

## Layout

    crates/prism-protocol/   the wire Frame + JSON/CBOR codecs
    crates/prism/            gateway lib + binary (actors.rs = the
                             echoes; identity.rs = accounts/devices)
    examples/echo-wasm/      the Rust-authored wasm echo (no_std, standalone)
