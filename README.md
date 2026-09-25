# prism — the connection plane

WS gateway hosting Aura actors, the entry point of the stateless-agent
stack (design: `docs/adr/0017-prism-connection-plane.md`, en/zh).
Built on the aura engine as a workspace sibling (`../../aura`,
`../../probe` by path — same triangle as aura's own cross-repo deps).

## Scope of this landing (echo plane, PLAN Phase 0+1 reduced)

Runs a gateway whose actor set is the echo in every embedded language —
the minimal example that proves each carrier end to end through a real
socket. Wire rules (ADR-0017 §4, honored here):

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

Echo actors (source: `crates/prism/src/actors.rs`; a language's carrier
absent from the build skips its echo with a boot note, never silently):

| type          | carrier  | form |
|---------------|----------|------|
| `echo_steel`  | steel    | `(define (echo_steel args) args)` |
| `echo_python` | python   | `def echo_python(args): return args` |
| `echo_nu`     | nushell  | `export def echo_nu [args] { $args }` |
| `echo_wasm`   | wasmtime | Rust `#![no_std]` cdylib, `examples/echo-wasm` (the full-power path, ADR-0026 §4 — pure echo needs no bridge: zero okm, zero deps) |

Deliberately NOT here (the rest of ADR-0017, planned as later phases):
device identity, login and connection sets, `/admin` actor upload,
`/probe/<alias>` mount (the probe gateway currently lives in
`aura/crates/engine`), `/assets`, the CBOR-vs-JSON DevTools panel. The
echo plane needs none of them; they ride prism PLAN Phase 1+.

## Run

```sh
cargo run                     # features default on: all four carriers
PRISM_ADDR=127.0.0.1:9000 cargo run
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
frames return error VALUES without closing. The codec pair itself
(dual-encoding maintenance contract: one model, two codecs, one
round-trip over every shape) is locked in `crates/prism-protocol`.

## Layout

    crates/prism-protocol/   the wire Frame + JSON/CBOR codecs
    crates/prism/            gateway lib + binary (actors.rs = the echoes)
    examples/echo-wasm/      the Rust-authored wasm echo (no_std, standalone)
