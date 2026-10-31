# Prism Modeling — the connection plane

The model of the plane in one place: who the participants are, what crosses the wire, who names what, and where state lives. It describes the SHAPE; the reasons behind each ruling live in the ADRs (0017 the wire, 0018 devices). When this document and an ADR disagree, the ADR is the record and this file is the defect.

> **Languages:** [English](MODELING.md) (primary) · [中文](MODELING.zh-CN.md)

## Participants

| name | what it is | who issues it |
|------|------------|---------------|
| **device** | one connection's identity, and the only reachability the venue knows | the venue, at connect (`?device=` anchors it across a reconnect) |
| **user** | the account a device is bound to; anonymous is a state, not a different device | the registry, at `login` |
| **token** | the credential naming (device, user); the page carries it and nothing else claims identity | the registry |
| **booth** | an application instance on the plane: declared by a name, told what happened by its own `@on` declarations | the application, at registration |
| **renderer (page)** | draws what arrives, sends `Outflow {dest, id?, data}` upstream | the venue (it is the page's origin) |
| **the venue (prism)** | forwards, adds and strips the marker, gates on declarations, fans out to devices | — |

## The wire

One vocabulary, marked. Everything that crosses the renderer leg is named `ui:<name>`; the marker is what stops a page from forging an internal event, and it is the venue's to add and strip — a page never writes it and never sees it.

| direction | the participant writes | what lands on the plane | what the recipient sees |
|-----------|-----------------------|-------------------------|-------------------------|
| uplink | `Outflow {dest, id?, data}` — a BARE name | `ui:<dest>`, payload `{device, sender, data}` | the booth is called by its own declared entry |
| downlink | `ui:<name>`, payload `{name, device, sender?, data}` | the same event | the page gets the payload with the marker stripped |

Two payload shapes are legal downstream, and the recipient decides what it can do with them:

- an **operation list** (`Vec<Content>`) is DRAWN — always an array, one array per message;
- **anything else is DATA** and lands on the vals plane at the slot named by the event, exactly as if the page had emitted that name itself.

Order inside one batch is semantics (a region's `set` precedes the patch that fills it), so a message is never split or reordered on the way.

**A sender hears nothing of its own event.** The venue excludes the sending device from the fan-out, which is why a page's own request never comes back to it — and why a booth's own speech names no `sender` when it wants the asking device to receive it.

## What addresses what

A **device** is the address; a **user** may be reached through the devices bound to it. There is no channel on this wire: a channel is a user-visible grouping (a group chat, or a one-to-one conversation using the same mechanism), and grouping is an application's business — it stores its own membership, answers "who is in here" itself, and turns a group into a list of recipients. The venue cannot enumerate a group, validate membership, or issue a channel id (ADR-0018).

## The application is a view

An application's WHOLE surface — what it shows and how it is configured — is the same two things: layouts and events. Nothing about management is a different kind of thing.

- **The initial layout is pushed, not requested.** A client cannot know when it is ready; the venue states what it has on connect and the client draws whatever arrives.
- **It is an EVENT that produces that layout, not a file and not a call.** The trigger is the connection notice described below (`ui:connect`); whichever booth declared it answers with frames. There is NO deployment-side layout configuration: the file form is gone (a layout with no logic is still an answer its carrier can produce, so a file only means the application is not answering), and the event name is fixed (`ui:connect`), so there is nothing left to name in an environment.
- **Why an event and not `invoke`.** `invoke` is CALL semantics: one named target, one answer the caller waits for — right for special cases, wrong for composition. Several booths own different parts of one application, so the trigger has to be something every interested booth can receive, each answering with its own frames.
- **Later layouts are ordinary traffic.** An application emits frames at any moment: `create`, `set`, `append`, `patch` against slots it declared earlier.
- **Expansion is a control and an event.** A layout names a control whose `dest` is an application event (a button "manage models", say); the event reaches the booth; the booth emits the frames that create or append that section; its controls emit further events. The venue cannot tell a management section from a chat rack and is never asked to.
- **So there is no admin surface per application**: no admin HTTP verb, no config file, no second origin. `/admin` on the venue is only about the VENUE (node approval, device revocation) — things about connections, devices and the plane itself.
- **A program is a client too.** Automation opens its own connection, gets its own device, and submits the same events with the same payloads; "operable by an agent" is a property of this shape, not an extra API.
- **The layout is the only thing a frame can be checked against.** A slot name is the sentence "my surface reads here", and only the party holding the layout can say whether that sentence is true — which is why the names inside a rendering are declared by whoever serves the layout, never guessed by the executor that draws into it.

## The connection lifecycle is an event stream

The venue reports what happened to a connection as events on the plane, and an application subscribes to the ones it cares about. Four notices, two pairs:

| event | when | payload | typically answered by |
|-------|------|---------|-----------------------|
| `ui:connect` | a device's connection is established | `{device}` | the orchestration booth, with the shell layout |
| `ui:login` | that device binds to a user (signup included) | `{device, user}` | whatever presents the account |
| `ui:logout` | the binding ends | `{device}` | the same |
| `ui:disconnect` | the connection is gone | `{device}` | cleanup; nothing is drawn from it |

`ui:init` is the name this replaces: it described the venue's own act rather than the fact the application cares about.

**A notice reaches the PAGE too, not only the application.** The venue is an emitter on this wire like anyone else, so the fact it states lands in the vals slot named after it — today `token` (the credential this connection must keep) and `connect` (that it opened), and a layout binding those learns what happened without asking. The two are SEPARATE emits, so the plane promises no order between them: a page binds both and assumes no sequence.

**The token rides the plane exactly once per issue, and only to its own device.** It is the page's credential, so the venue says it on the connection that owns it (`token {device, token}`) when it is minted or rotated — never in a batch, never to a second device. An application that must act on the user's behalf addresses the DEVICE that is logged in; the venue resolves that device to a user at every uplink, and its auth gate is what actually holds the permission. **Which is why no fact carries the user**: the page would be a second home for an answer the venue gives at the moment it is needed, and two homes disagree eventually.

**One clock, and it is the device's.** A token has no expiry of its own; `Activity` (one timestamp per device) is what decides whether a credential still carries permission. A device silent past the window connects as anonymous, is told why (`notice` with `reason: idle`), and keeps its id — that is what lets a login restore the same device with the name it already had, instead of minting a new one every timeout.

**Facts are named as they happened; requests are imperative.** `ui:connect` and `ui:login` are things the venue OBSERVED, so the application only has to know about them; `ui:turn.submit` is a thing a client WANTS, so it is named as an act. The difference is a word on the wire, and it is why a lifecycle notice never reads like a command an application might refuse.

## Islands: one application, several booths

The case this model exists for, spelled out.

An ORCHESTRATION booth emits an island-mode frame: a layout framework with named regions, each carrying the slots that will exist inside it. It then emits the business events those regions are about. Each booth that declared one of those events answers by emitting its own layout into the slot prepared for it — a chat rack, a force graph, a settings panel — and from then on keeps talking to that region directly.

Why this is a modularity claim and not just "frames compose":

- **One description, two sides.** The layout a region shows is emitted by the booth that owns the logic behind it, so a part cannot draw differently from what it does — there is no second description to drift.
- **The trigger is an event, so the number of parts is open.** A new booth joins by declaring the event; neither the venue nor the orchestration booth enumerates the application's parts.
- **Regions are slots, so a part can be replaced without touching the shell.** The island frame names positions; who fills them is a routing question on the plane.
- **It is the same in one process or many.** Every step is ordinary traffic: a part written by someone else, running elsewhere, answers the same event the same way.

**Navigation needs no separate machinery.** A nav bar is just another named region in the shell, and a link is just a control inside it whose `dest` names an event; whoever declared that event answers with frames. So there is no such thing as navigation here: the page stays the one page, and what changes is the layout it receives; the only thing that leaves the plane is a link pointing outside it — that is not navigation, that is leaving.

**It is also why "front-end microservices" hold here and held nowhere else.** The earlier attempts got the split right and paid for it everywhere else — sandboxes, version arbitration over shared dependencies, who takes over the router — the business was separated and the implementation got heavier. Here the split sits at the message layer: an island is a layout that declared an event, and isolation is what the wire gives.

So there is no separate "front-end microservice" problem left to solve: an application's front end and back end are one development unit, microservice-shaped from the start, with the view as one of its composable members — splittable, replaceable, assemblable, with no need to build a whole page first and then take it apart. The unit can be finer than "one front-end application" too: one region, one event. And the side writing an island never has to know who else is there, nor share a build with them.

## Where state lives

| owner | what it owns | what it must never hold |
|-------|--------------|--------------------------|
| the venue | connections, devices, users, tokens, the routes it was told to serve | an application's vocabulary (a model, a rule, a setting) |
| the application | its rows (in its own namespace), its layouts, its event vocabulary | a copy of the venue's reachability facts |
| the page | the token and what it was handed | anything durable: a page that stores truth is a second source of truth |
| the platform (aura) | the field: types, routes, the embedded store an application declares | business policy for any application |

## Two renderings of one application

The same application may speak to a renderer and to a program. The difference is the OUTPUT SHAPE, not a second implementation: frames for a surface that only draws, raw deltas for a client that reads. The shape is the caller's choice on the way in, the names inside it are the caller's declaration, and both travel the one wire — the venue strips the `ui:` marker either way, so a program sees the plain names it already speaks.

## What this model refuses

- A page that renders business knowledge it was not handed (fields, defaults, ranges) — the renderer stays dumb by construction.
- A router of the page's own — where a click goes is said by the layout the application emitted, not by a list of destinations the page keeps.
- A transport that learns an application's vocabulary — the moment prism knows what a model is, every application change is a gateway change.
- A second surface for administration — one vocabulary, one auth story, one client for the product and its settings.
- A layout that exists only as a deployment file, when the application behind it could state it itself.
