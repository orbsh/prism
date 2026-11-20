# 0020 — a token is minted for one use, and names are given at minting

Status: Accepted (2026-11-20)

## Context

Prism mints a node token (`POST /admin/nodes {alias}`), stores only its SHA-256 digest, and shows the token once. The token rides the dial (`?token=…`); the registry answers `registered | pending | conflict`; one alias holds one approved key at a time, and approving a second key demotes the first (the rotation rule in `nodes.rs`).

Two gaps showed up once the venue gained a node pool and placement (aura ADR-0053):

- **The token was bound to a NAME, not to a machine.** Anyone holding a token could use it from any box: the registry checked what was claimed, never what was presenting it. So "one token, one node" was a convention, not a fact.
- **Placement needs human names, and there was nowhere to type one.** A person asks for "copy the files on my laptop to the server". They cannot ask by serial — that is not something a human says, and it is not something an AI should have to learn either. Real addressing stays by id; the name is how work is REQUESTED.

## Decision

1. **A token is minted for one use: the first machine that successfully registers claims it.** The registry row gains `machine`, written on the first APPROVED registration and never changed after. Every later registration of that token must come from the same machine; another box presenting it is `conflict`, and the reason names both machines. Restarting on the SAME machine reconnects normally — "one use" is a fact about hardware, not about process lifetime, which is why this is not single-use consumption (a node that could not remint its own token would turn every unattended restart into an operator task).

2. **A node that cannot identify its hardware is not bound.** It registers (empty `machine`), the registry says out loud that the token stays unbound and therefore is not confined to one box, and the first identified registration after that binds it. A law that silently refuses boxes without SMBIOS would make a hardware detail decide whether a node can exist.

3. **Labels are given at minting, by the person who knows the machine.** `POST /admin/nodes {alias, labels, owner}`: `labels` are names as typed — `MyLaptop`, `xxServer` — and `owner` is the user they belong to. Stored bare, one per line, so a rename is a rename and the row keeps meaning what the operator said.

4. **The user prefix is added at admission, not in the registry.** The execution layer (the effector mount, on the way into the venue's pool) turns `MyLaptop` into `{owner}:MyLaptop`. The registry keeps names; the venue matches on addresses. Two users' `MyLaptop` therefore cannot collide in a placement, and neither user has to know the prefix exists.

5. **Names never address anything.** Real addressing is by machine id (effector ADR-0006), which the user never sees. A label is a selector for an OPERATION — "my laptop", "the server" — and the mapping from that word to hardware is resolved at admission, which is the only place that knows both.

## Rejected

- **Strict single-use consumption** (the token is destroyed by the first successful registration). Restart would then need a human or a boot-time minting step; a container node restarting is ordinary, not exceptional.
- **Let the node propose its own label** in `Register`. Same reason as effector ADR-0005 §2b: a label is a guessable, shared word, so a node saying `MyLaptop` is indistinguishable from a node taking over somebody's laptop. Names are the admitting party's, minted with the token.
- **Address by label** (a call that names the label directly). It would make a human word a routing key, so a rename would orphan history and two users' names would need a syntax at every call site. The label becomes an address exactly once, at admission.
- **Store labels in a second table keyed by node id.** The payload is fixed-width TLV with no sequence type; a line-joined string in the row keeps the split in ONE function (`Node::label_list`) instead of adding a collection, an index and a join nobody asked for.
- **Bind the token to the alias at minting** (i.e. refuse a different alias). The alias is the name a connection answers to; the machine is the box. Binding to the alias would forbid a rename, which is a legitimate operator action.

## Consequences

- **A leaked token is a token for one box.** An attacker who obtains it cannot use it from elsewhere; the registry refuses with both machines named.
- **A token minted before this record is unbound until its first approved registration**, then binds. Existing rows decode unchanged (absent fields take `Default`).
- **Renaming a machine is minting a new token**, which is what "one token, one use" means operationally: the old row is revoked, the new one carries the new name, and the machine id keeps the history continuous.
- **The admin surface is the place names are born**, so it is the place an operator's vocabulary is audited. `GET /admin/nodes` reports `labels`, `owner` and `machine` for exactly that reason.
- **Effector's frame changed for this to be possible** (`machine`, effector ADR-0006). Nothing about labels went on the wire.

## Tests

- `crates/prism/src/nodes.rs` — `a_token_is_bound_to_the_first_machine_that_presents_it` (first approved registration binds; the same machine reconnects; another machine is a conflict whose reason names both), `a_node_that_cannot_name_its_hardware_stays_unbound` (empty machine registers, binds nothing, and the first identified registration binds), plus the existing lifecycle updated for the three-argument verdict.
- `crates/prism/tests/admin_nodes.rs` — the mint → approve → verdict lifecycle over HTTP, with `machine` supplied at the seam.
- `crates/prism/tests/effector_handshake.rs`, `effector_mount.rs`, `runner_mount.rs`, `turn_start.rs`, `resource_path.rs` — registration carries `machine`, and presence is asserted through the venue pool (`realm_set.pool()`).

## Related records

the rotation rule in `crates/prism/src/nodes.rs` (approving a second key demotes the first), effector ADR-0003 (one token at the dial), effector ADR-0005 §2b (labels are the admitting party's word), effector ADR-0006 (the node states the box), aura ADR-0053 (the pool, the prefix, and the filter).