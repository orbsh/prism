# 0019 — The page frame is a file, and a region is an address

Status: Accepted (2026-11-09)

## Context

Until now the page was drawn entirely by whichever booth answered `ui:connect`. gravity's answer was ~100 lines of Rust assembling a navigation bar, a pager, an auth region, a notice area and two pages, and emitting it as one `create` plus a handful of `append`s.

That made three things true, none of them intended:

1. **A layout decision lived in an application.** Which regions exist on this page, what the bar says, where the auth screen goes — all of it belonged to the operator, and none of it could change without a rebuild of a booth.
2. **One booth could not see another's regions.** A page assembled by one application can only contain that application's surface. A second island — the grant page, say — has nowhere to draw, because the thing that would hold it is drawn by someone who has never heard of it.
3. **Every application had to draw a page.** "Answer `ui:connect` with a `create`" is a rule that only makes sense while exactly one application exists.

A front-end meta-framework solves this by bundling: it packs several stacks into one artifact and leaves the boundaries to runtime convention. What it never provides is the two things that make islands work — an address a region can be written to without knowing anything else, and an update unit that is the same thing as the isolation unit.

## Decision

**A frame is a FILE the operator writes; a region is a SLOT named after its booth.**

- `PRISM_SHELL` names a JSON file holding one accrete document. prism reads it at boot (an unreadable file or a list-of-actions is a boot error, not a silent no-shell), and on every renderer connection pushes one `create` whose data is that document — **before** `ui:connect` is said, because a booth that answers with a `set` needs the slot to exist.
- A region is declared in that file as `{"bind": {"value": {"kind": "source", "source": "island:<booth type>"}}}`. The slot name IS the address: no mapping table, no registration, nothing that can disagree with the file. A deployment that renames a booth renames one string.
- **The prefix is the venue's, not the booth's.** A hosted booth addresses its own surface with the EMPTY slot and its own names (`pages`, `notice`); the view leg rewrites every slot of that booth's frames to `island:<the page's name for it>/<slot>`, where the page's name is the first segment of the event (`ui:gravity-model.layout` ⇒ `gravity-model`) — not the aura booth type, which is not something a page knows. A slot already written as `island:…` passes through unchanged: writing into another region takes knowing its name, and no booth is told one.
- Every delivery carries `shell: true|false` at top level: `true` means the venue drew the frame, so a `create` here would replace the root and erase the navigation and every other island in it; `false` means the booth is the page, as before.
- **The frame's dynamic surface is two slots, filled by the venue.** `user` holds who is looking (device alias, or the short id, marked `(guest)` when no account is bound) and `nav` holds one item per island, labelled by the frame's own `attrs.label`. Both are written on every connection, BEFORE `ui:connect`. Both are facts a booth is never told — identity is resolved server-side at every uplink (ADR-0018), and the deployment's regions are the frame's business.
- **The switch is fluxen's `pages`, driven locally.** The frame declares `regions: [{booth, label}]` (a declaration that renders nothing); the venue supplies the two rows that make it work — a `nav` item and a `pages` row with the same id, the row holding the region's binding — and the frame wires a `select` and a `pages` node to the same local `page` value. Switching therefore costs no round trip for a page already visited: the first click that lands on an island reports it (the `local` binding's `report`, fluxen ADR 0028), because that is the one fact only the venue can hold — it remembers where this person stood and calls that booth alone; after the first visit, clicks stay local. The rows come from the venue because a page's rows can only come from a list slot; a static child would be a second way to put a page on screen, and the one that cannot be switched away from.
- **An application's initial layout is NOT a file, and cannot be.** A chat surface answers a connection with a layout and then with what that layout should show — the shape depends on the data (history, presence, first visit), so the two are one act of initialisation. A file is fixed at boot and knows nothing about the connection. That is the split: a frame is fixed and shared, an application's surface is per-connection.
- Absent `PRISM_SHELL` is legal and means the old arrangement. `crates/prism/assets/shell.json` is the reference frame.

## Considered and rejected

- **A shell booth.** It would draw a constant, and a constant that costs a booth also costs a lifecycle, a receive declaration and a failure mode (a booth that did not register leaves the page frameless). A file has one failure mode, at boot, in front of the operator.
- **A shell DSL in prism's config.** KDL reads better for people, but the thing being written is a view, and the wire already has a shape for views. Introducing a second spelling of the same tree buys nothing and doubles what has to stay in step.
- **`create` for every island into the same root.** Islands would overwrite each other, which is exactly the failure the slot boundary exists to prevent.

## Consequences

- The frame's owner is the operator, not a booth. Changing navigation is editing a file and restarting prism — no crate is involved.
- An island is an ordinary `set` into a name the venue derived. That means the isolation unit and the update unit are the same object: the renderer re-renders exactly the nodes bound to the slot it wrote. And the isolation is a MECHANISM rather than a discipline — an application that writes `pages` gets `island:<its name>/pages`, so reaching another island's region requires spelling a name nobody gave it.
- The address convention replaces configuration, and the boot log states what it found (`prism: page frame ← … (islands: …)`), so a deployment that meant to have navigation says so.
- gravity's shell and screen still build the tree in code; only its ADDRESS is chosen by the flag. The tree remains gravity's business, which is correct — what an application draws inside its region is its own.