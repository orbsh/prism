# Prism handshake plan (Phase 1.8 step 2, second half) — unblocked 2026-10-29

## Status

Shelved 2026-10-12, unblocked 2026-10-29. The shelving reason is gone on both halves:

- **The effector refactor landed.** The bgi-guest → induction migration is complete and the tree is clean; the mount this handshake runs on already exists (`crates/prism/src/effectors.rs`).
- **The frame home is ruled.** `~/world/induction/docs/ADR-0001` Consequences, "Resolved 2026-10-13 (batch 3 as landed)": `HostOp` (WS, serde-tagged, id-paired) and the pipe `HostFrame` (in-band, serially paired) are **two transport encodings of one `Arm` vocabulary**, not two protocols to collapse; `induction-host::route_frame` is the codec- and transport-independent core both hosts drive. So a `Challenge`/`Register` reshape is effector-protocol's own transport vocabulary — it collides with nothing in induction.

Nothing else blocks it. The work is the approved plan below, unchanged.

## Where it stands (2026-10-29)

- **effector half LANDED** (effector ADR-0002, tests green): `Challenge`/signed `Register`/
  `Refused`, `keygen` + `register` CLI, `private_key_path`.
- **aura's local gateway LANDED**: sends the challenge, verifies the signature, no registry
  (`alias_takeover` + `remote_effector` green — the latter drives the REAL effector runtime, so
  the whole sign/verify path is proven against a host).
- **prism's mount LANDED AND VERIFIED** (2026-10-29, re-measured 2026-10-31): `effectors.rs` sends
  the nonce, verifies in both postures (`open` = no registry, never no proof, and it logs that), and
  gates `required` on `NodeStore::verdict` with a stated `Refused` for `pending`/`conflict`.
  `tests/effector_handshake.rs` is SIX cases — required admits an approved key, refuses an
  unapproved one, refuses a different key for an enrolled alias, refuses a forged signature in BOTH
  postures, `open` admits a key no registry knows, and a frame claiming another alias closes
  without `Registered`; `tests/admin_nodes.rs` is the approval HTTP face, 4 cases. Do not report the
  handshake as done until those pass: `cargo test -p prism --test effector_handshake --test
  admin_nodes -- --test-threads=1`.

  The 2026-10-29 entry above said the crate did not compile — that was fluxen's `content::Message`
  removal, long since landed. The 2026-10-31 re-run hit a SECOND, unrelated staleness: prism's
  `Cargo.lock` (gitignored here) pinned okm `ca827287` while aura's own lock had moved to
  `c4961c85` and aura's source adapted to it (`AccessMethod` lost `version`), so `aura-realm` failed
  to compile inside prism's build. `cargo update -p okm-core -p okm-derive -p okm-dynamic -p
  okm-entry -p okm-wire` fixes it locally; nothing in either repo is at fault and no lock is
  committed.

## Done already (prism f5837d2)

`/effector/<alias>` mount LANDED: `crates/prism/src/effectors.rs` serves the mounted gateway (Register handshake, Result correlation, ctx-bridge via `aura_engine::host_wire`, presence guard). Path segment = alias claim, Register frame must repeat it, mismatch closes. Trust posture NOT consulted yet — that is this work. Aura keeps `serve_effectors` (independent port) for local mode; aura remote_effector/alias_takeover tests green.

## What is actually left: step ③, and it is deferred on purpose

Phase 1.8 had three steps. Step 1 (the registry + verdict + posture + curl-first admin face) and
step 2 (this handshake, both ends) are landed and verified. Step ③ — **node records under the
ACCOUNT**, so enrolment/approval/revocation ride Phase 1's auth session instead of an operator's
curl — is the only piece left, and `docs/PLAN.md`'s "Multi-tenant auth (keys/accounts): single-user
first; add auth layers only with the first external consumer" is the standing reason not to build it
yet. The wire consequence to keep in mind when it is taken: a node's identity stays (alias,
public_key) — the handshake never learns a user, so ownership can only govern WHO MAY ENROL, never
the verdict's shape.

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