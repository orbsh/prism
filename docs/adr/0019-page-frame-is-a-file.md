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
- Every delivery the venue makes carries `shell: true|false` at top level. A booth that receives `true` writes its surface into `island:<its own type>`; on `false` it draws the whole page as before. The booth never guesses, because guessing wrong is silent: a `create` inside a frame replaces the root and erases the navigation and every other island with it.
- Absent `PRISM_SHELL` is legal and means the old arrangement. `crates/prism/assets/shell.json` is the reference frame.

## Considered and rejected

- **A shell booth.** It would draw a constant, and a constant that costs a booth also costs a lifecycle, a receive declaration and a failure mode (a booth that did not register leaves the page frameless). A file has one failure mode, at boot, in front of the operator.
- **A shell DSL in prism's config.** KDL reads better for people, but the thing being written is a view, and the wire already has a shape for views. Introducing a second spelling of the same tree buys nothing and doubles what has to stay in step.
- **`create` for every island into the same root.** Islands would overwrite each other, which is exactly the failure the slot boundary exists to prevent.

## Consequences

- The frame's owner is the operator, not a booth. Changing navigation is editing a file and restarting prism — no crate is involved.
- An island is an ordinary `set` into a name the file wrote. That means the isolation unit and the update unit are the same object: a booth cannot reach another's region, and the renderer re-renders exactly the nodes bound to the slot it wrote.
- The address convention replaces configuration, and the boot log states what it found (`prism: page frame ← … (islands: …)`), so a deployment that meant to have navigation says so.
- gravity's shell and screen still build the tree in code; only its ADDRESS is chosen by the flag. The tree remains gravity's business, which is correct — what an application draws inside its region is its own.