# Prism handshake plan (Phase 1.8 step 2, second half) — shelved 2026-10-12

## Why shelved

Effector is mid-refactor (bgi-guest → induction migration, main tree dirty). The frame home (effector-protocol vs induction-protocol) is UNDECIDED — no frame changes in effector until that ruling exists.

## Done already (prism f5837d2)

`/effector/<alias>` mount LANDED: `crates/prism/src/effectors.rs` serves the mounted gateway (Register handshake, Result correlation, ctx-bridge via `aura_engine::host_wire`, presence guard). Path segment = alias claim, Register frame must repeat it, mismatch closes. Trust posture NOT consulted yet — that is the shelved work. Aura keeps `serve_effectors` (independent port) for local mode; aura remote_effector/alias_takeover tests green.

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

New git worktree off effector `f94ad9c` (`git worktree add ../effector-handshake -b feat/handshake`); prism path-deps the worktree. Merge after refactor lands; `crates/protocol` should be near-zero conflict (refactor touches runtime/carrier + workspace files) — recheck if induction moves the frame vocabulary.

## Environmental facts (costly to rediscover)

- prism tests need BOTH `RUSTFLAGS="-L native=/nix/store/lb41b0anx1f98y9y5s9mdv97gjgsq740-python3-3.14.7/lib"` AND `PYTHONHOME=/nix/store/lb41b0anx1f98y9y5s9mdv97gjgsq740-python3-3.14.7` — missing PYTHONHOME = embedded python carrier fails silently, observed as a HANG in turn_e2e, not an error.
- prism workspace 44/44 green at shelving time (`cargo test --workspace -- --test-threads=1`).
- `futures_util::stream::SplitStream` wraps a BiLock — no get_ref; capture peer addresses BEFORE splitting the WS.
- prism `Gateway.engine` is `pub` — tests reach the realm directly for registrations.
