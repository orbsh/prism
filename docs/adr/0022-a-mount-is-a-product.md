# 0022 — A mount is the product of a vocabulary and an approved alias

> **Languages:** [English](0022-a-mount-is-a-product.md) (primary) · [中文](0022-a-mount-is-a-product.zh-CN.md)

**Status:** Accepted (2026-11-20)

## Context

A node becomes callable through a mount: one `Remote` type per (alias × operation language), whose source is a fixed runner (gravity ADR-0001). The declarations lived in `PRISM_MOUNTS`, one line per alias, replayed at every boot.

Two things went wrong with that shape, and they went wrong in opposite directions:

- **A machine that dialled in without a line was in the venue and uncallable.** The pool held it, a placement could reach it, and no type existed to address it. A personal laptop — the machine the whole node story is for — had to be written into a deployment manifest to be used at all.
- **The line carried two facts with different owners.** Which languages this installation can run, with which runner artifact, is a fact about the INSTALLATION: it is the same for every node and it ships with the deployment. Which machines are nodes is a fact about the registry: a row is approved or it is not. One line per alias forced the second list to be authored again in the first list's vocabulary.

## Decision

### 1. The vocabulary is the deployment's, declared once

`PRISM_LANGUAGES` is a JSON array of `{language, carrier, runner, encoding?, requires?}` — the operation languages this installation runs, each with its carrier, its runner artifact, its frame codec and what isolation it requires. Absent means this installation runs no node code, which is legal and said at boot. Malformed is a boot error: it is a deployment fact, never a default (ADR-0001's rule, unchanged).

### 2. The alias list is the registry's, and it already existed

One mount per (APPROVED row × vocabulary entry), type name `{alias}.{language}`. Nothing new is stored on the row: approval is the whole fact. A pending row mints nothing and a revoked row mints nothing — the same discipline machine binding follows (prism ADR-0020).

An alias may not contain `.` — the type name is `{alias}.{language}`, and a dotted alias would let two different pairs spell one name. Refused where names are minted (`POST /admin/nodes`), not at the mount.

### 3. Every node serves the whole vocabulary; a missing runner is the environment's problem

There is no per-node language selection. A machine gets every language the installation declares, and if it lacks the artifact a language needs, the CALL fails — the operator or the user fixes the machine. The alternative (ticking languages per node at minting) buys an error caught earlier at the cost of a step every operator pays for every machine, and a manual list that can be wrong in the quieter direction: the language exists, the runner does not.

### 4. Registration happens at boot, at approval's next boot, and on a dial

- **Boot** derives and registers every approved row's mounts: the listing is honest before any node connects (`registered: true`, `node_connected: false`).
- **A dial** tops up the table for an alias it has not seen. In `open` posture that is the only alias list there is — the network is the boundary, and the log says the types were minted from a dial. In `required` posture it covers a row approved after boot, which would otherwise wait for a restart.
- Derivation is idempotent: a second dial of the same alias adds nothing.

### 5. `PRISM_MOUNTS` retires

Its two halves went to their owners (§1, §2). Keeping it would put a second answer beside the registry's, which is the drift this record exists to remove.

## Rejected

- **Keep the per-alias manifest as an override** beside the derivation. Two lists again, and the override list would silently win over the registry — a deployment could declare a mount for an alias whose row was revoked.
- **Let a dialling node declare its own languages** (from `Frame::Register`'s carriers). A node saying "I can run python" is a guessable claim, exactly like a self-declared label (effector ADR-0005 §2b); ADR-0001's ruling that no data path mints mounts keeps it out. The carriers the dial announces still have a job: the venue compares them against the vocabulary and states the drift, without refusing.
- **Tick languages per node at minting** (the earlier fork). Rejected by the ruling in §3: it moves an environment failure to a checkbox that can lie.
- **Derive on demand only** (first call mints the type). A caller reads what is callable before it calls; a table that fills itself at call time answers every listing with "nothing yet".
- **Unregister a revoked row's mounts.** The types stay and their node is gone: calls answer "not connected", and the listing reports `node_connected: false`. Unregistering would also have to handle the case where a job's declaration set was resolved against the type a moment earlier.

## Consequences

- **A machine is callable the moment its token is approved**, and a personal laptop needs no manifest line.
- **The vocabulary is one list for the whole installation**, so "which languages can this venue run" has one answer, and adding a language is one deployment change.
- **The call-time failure is the design**: a node without python's artifact sells a python mount that fails. The listing cannot know (it would have to ask every node what it installed and believe the answer); the failure names the mount, and the environment gets fixed.
- **A type name encodes two facts nobody parses**: `{alias}.{language}` is readable for an operator and treated as opaque by everything else (`ctx.invoke` takes a name, not a path).
- **In `open` posture a dial mints types.** That is the posture's trade, stated at the dial: whoever can reach the gateway is a node, so an alias that dials becomes callable. `required` posture mints only from approved rows.

## Tests

- `crates/prism/tests/mount_listing.rs` — `a_mount_is_the_product_of_the_vocabulary_and_an_approved_alias`: nothing callable before an approval; two mounts per language after it, with `registered` true and `node_connected` false; presence flipping in the pool; `open`-posture derivation idempotent on a second dial; a vocabulary missing `runner` failing as a boot error while the installed table stays untouched.

## Related records

gravity ADR-0001 (the mount, the runner, and "no data path mints mounts"), prism ADR-0021 (the venue publishes the table it holds — this record is where the table comes from), prism ADR-0020 (the token, the labels, and approval as the act that admits), aura ADR-0053 (the pool that answers presence), effector ADR-0005 §2b (a node's claim about itself is not a declaration).