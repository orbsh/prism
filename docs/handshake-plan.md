# Prism handshake plan (Phase 1.8 step 2, second half) — unblocked 2026-10-29

## Status

Shelved 2026-10-12, unblocked 2026-10-29. The shelving reason is gone on both halves:

- **The effector refactor landed.** The bgi-guest → induction migration is complete and the tree is clean; the mount this handshake runs on already exists (`crates/prism/src/effectors.rs`).
- **The frame home is ruled.** `~/world/induction/docs/ADR-0001` Consequences, "Resolved 2026-10-13 (batch 3 as landed)": `HostOp` (WS, serde-tagged, id-paired) and the pipe `HostFrame` (in-band, serially paired) are **two transport encodings of one `Arm` vocabulary**, not two protocols to collapse; `induction-host::route_frame` is the codec- and transport-independent core both hosts drive. So a `Challenge`/`Register` reshape is effector-protocol's own transport vocabulary — it collides with nothing in induction.

Nothing else blocks it. The work is the approved plan below, unchanged.

## Done already (prism f5837d2)

`/effector/<alias>` mount LANDED: `crates/prism/src/effectors.rs` serves the mounted gateway (Register handshake, Result correlation, ctx-bridge via `aura_engine::host_wire`, presence guard). Path segment = alias claim, Register frame must repeat it, mismatch closes. Trust posture NOT consulted yet — that is this work. Aura keeps `serve_effectors` (independent port) for local mode; aura remote_effector/alias_takeover tests green.

## Approved plan

Sequence: connect → host `Challenge{nonce}` → effector signs nonce+alias (ed25519) → `Register{node_alias, public_key, signature, carriers}` → host verifies + `NodeStore::verdict(alias, pk)` three-way (accept / pending queue / conflict refuse).

1. Effector CLI subcommands (`crates/runtime/src/main.rs`; CLI form only, library form untouched):
   - `effector keygen [--out <file>]` — ed25519 pair, private key file 0600, public key printed base64 → operator pre-registers via `POST /admin/nodes`.
   - `effector register <config.json>` — one-shot handshake, print verdict, exit. Registration is a network FACT (persistent in prism registry); serving is runtime state — a restart re-derives trust per connection via challenge, no re-register.
   - No subcommand = current behavior (load config, run wrapper).
2. effector-protocol: add `Challenge{nonce}` (host→effector); reshape Register: `credential: String` → `{public_key, signature}` (signature over nonce+alias).
3. effector-config: `credential_env` → `private_key_path` (file perms OS-managed; keys don't ride env).
4. prism `src/effectors.rs`: `required` runs the full handshake; `open` keeps direct pass + disclosure log.
5. credential field retires both ends (signature replaces the shared token).
6. Tests: CLI keygen+register walk-through; required three-way; wrong-signature refuse; open regression. Deps: ed25519-dalek + rand both ends.

## Working arrangement

The original worktree arrangement (off effector `f94ad9c`) is stale — effector has moved through the induction migration since. Branch off current `main` in each repo and land the two ends together: the frame shape changes on both sides of one connection, so a one-sided merge leaves the mount unable to register anything (the posture's `required` mode would answer every node as keyless).

## Environmental facts (costly to rediscover)

- The `PYTHONHOME` note below is HISTORICAL: the embedded python carrier is retired (both booths are Rust induction guests), and the PLAN records "no carrier feature gate, no PYTHONHOME/RUSTFLAGS for this plane". Builds that still enable the `python` feature of aura's engine need the `RUSTFLAGS="-L native=…/python3-3.14.7-env/lib"` search path; the handshake work itself touches no carrier.
- prism workspace 44/44 green at shelving time (`cargo test --workspace -- --test-threads=1`).
- `futures_util::stream::SplitStream` wraps a BiLock — no get_ref; capture peer addresses BEFORE splitting the WS.
- prism `Gateway.engine` is `pub` — tests reach the realm directly for registrations.