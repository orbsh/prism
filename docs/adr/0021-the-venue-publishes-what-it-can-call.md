# 0021 — The venue publishes what it can call

> **Languages:** [English](0021-the-venue-publishes-what-it-can-call.md) (primary) · [中文](0021-the-venue-publishes-what-it-can-call.zh-CN.md)

**Status:** Accepted (2026-11-20)

## Context

A node is callable through a mount: one `Remote` type per (alias × operation language), whose source is a fixed runner (gravity ADR-0001). The declarations are derived from the deployment's language vocabulary and the approved node rows, and replayed at every boot, because `Remote` definitions are not persisted (ADR-0022).

That leaves the table with no home a caller can ask. gravity carries its own copy in the turn's args ("the mount table travels in the turn's args, so every caller carries it"), and gravity ADR-0001 already names the consequence as open: *prism already holds the same table, so each further client is a drift source*.

Two failures follow from a table nobody can read:

- **A client cannot tell "declared" from "callable".** A declaration that failed to register is in the manifest and in the client's copy; the type is not in the engine, and the call fails long after the confusion started.
- **A client cannot tell "registered" from "present".** A registered mount whose node has gone home answers "not connected" — mid-job, with an op already failing. Presence is a fact only the venue has, in its pool (aura ADR-0053).

## Decision

### 1. The venue answers for the table it holds

`GET /admin/mounts` returns every declaration it holds, with three facts per row and a list of refusals:

```
{ "mounts": [ { "type", "alias", "language", "carrier", "runner",
                "requires", "encoding",     // resolved, not echoed: absent requires IS the container
                "registered",               // the engine took the type
                "node_connected" } ],       // the node behind it is in the pool right now
  "refused": [ "mount [x] (alias y) did not register: …" ] }
```

`registered` and `node_connected` are deliberately separate rows of truth: the first says the venue minted the type, the second says somebody is listening. A caller choosing a mount needs both, and only the venue knows the second.

### 2. The table is gateway state, installed once

The manifest is read by `Gateway::install_mounts`, which keeps what it installed. Before this, the mount list existed only as a boot-time side effect: two places that register mounts are two answers to "what is callable", so there is exactly one, and it is a method on the venue rather than a constructor.

A manifest that fails to parse is still a boot error (gravity ADR-0001: a declaration is a deployment fact, never a default), and a failed install leaves the venue holding what it already had — half a table would be worse than none.

### 3. Callers read; nobody copies

The open item in gravity ADR-0001 is resolved in the direction of reading: a client that needs the table asks the venue for it. Injection into the turn's args was the other candidate and is rejected below.

Adopting it in gravity (dropping the table it carries today) is gravity's own change, not this one; what this record fixes is that the venue has an answer to give.

## Rejected

- **The venue injects the table into the turn's args when it routes `ui:turn.submit`.** The venue would have to know the shape of an application's payload to write into it — the same boundary that keeps gravity from being taught the venue's internals, crossed in the other direction. It also makes the table a value the venue authors inside someone else's structure, where a second application served by the same venue would need the same arrangement.
- **Every client keeps its own copy** (today's shape). ADR-0001 already recorded this as a drift source; naming it once more does not fix it.
- **Publish only the declarations** (the manifest text, qualified by nothing). The row a caller must not use is exactly the declaration that failed to register, and the manifest cannot say so.
- **Publish only what registered.** Then a mount that is live but whose node is away looks callable, and the caller learns by failing an op.

## Consequences

- **One table, one home.** A change to the deployment is visible to every client at once, and the boot log links to the same page a client reads (`… refused — see /admin/mounts`).
- **A malformed declaration is still loud**: the boot fails, or the refusal is on this page. Nothing is skipped silently.
- **Presence is a snapshot, not a promise.** A node may leave between the read and the call; the listing narrows the choice, it does not reserve anything.
- **The listing is the operator's too**: it answers "why did my call say no such type" and "why did it say not connected" from one page.

## Tests

- `crates/prism/tests/mount_listing.rs` — `the_venue_publishes_what_it_can_call_and_what_it_could_not`: every declaration is listed, `requires` reported as resolved, `registered` true and `node_connected` false before the node dials, presence flipping to true when a record enters the pool, and a manifest missing `runner` failing as a boot error while the installed table stays untouched.

## Related records

gravity ADR-0001 §"prism" and its open item (the mount table and who carries it), aura ADR-0053 (the pool, which is where presence is decided), aura ADR-0027 (code by reference — untouched: the runner is the mount's source), prism ADR-0017 §7 (the `/admin` prefix this rides).