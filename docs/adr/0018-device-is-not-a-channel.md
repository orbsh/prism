# 0018 — A device is a connection, a channel is business

Status: Accepted (2026-10-29)
Supersedes: nothing. Corrects a claim in this repository's earlier design notes and in the
2026-10-29 wire work, where `channel_id` was treated as a venue concept.

## Context

The connection plane (ADR-0017) assigns one **device** id per connection and binds it to a
**user** account. Both facts are verified here: the device is what the socket was given at
connect, the user is the registry's answer to `login`, and the auth declarations are enforced
against both before an event is delivered.

The wire also carries a field named `channel_id`. Today it is written as the device id
(`ui.rs`, the venue's own speech) and read back by comparing it to the receiving connection's
device:

    if target.is_some_and(|t| t != c.device) { continue; }

That is not a channel — it is a device wearing the name, and the name invites the wrong model. A
channel is a **user-visible grouping**: a group chat, where a one-to-one conversation is the same
mechanism with two members (which is why both are called a channel). Membership, creation (an
explicit "create group", or implied by starting to talk to someone), invitations later in the
conversation, and whether any of this exists at all are properties of an APPLICATION, not of a
transport. One application may support only one-to-one and simply store the pair of users'
conversation; another may have groups and no one-to-one channels at all; the resident-application
shape described in the wiki (`液态化`) has neither, because its view is opened by one connection
and answered on that same connection.

## Decision

1. **The venue knows devices and users. That is the whole of its identity model.** Everything about
   reachability beyond "this connection" / "this account" is somebody else's state. This pair is
   the stable part: it changes when the transport changes, not when an application's product
   decisions change.

2. **A channel is a booth's concept.** A booth that implements groups holds the membership it needs
   in its own namespace, keyed however its business dictates, and answers "who is in this group"
   itself. prism does not know that a group exists, cannot enumerate one, and never validates
   membership.

3. **`channel_id` is not a venue field, and leaves the wire.** Fan-out is an ADDRESS, not a
   membership test: an event that wants several recipients names them (device ids, or the users
   that resolve to them). "Send to everyone in channel 7" is a sentence only the booth that owns
   channel 7 can say, so it is the booth that turns a group into a recipient list.

4. **What the venue still guarantees,** and nothing more: a device id is the address of a live
   connection; an anchored id (`?device=`) survives a reconnect; the device↔user binding is the
   registry's; the auth declaration gates delivery before it happens; a sender is excluded from
   the fan-out of its own event.

## Consequences

- The `channel_id` comparison in the downlink goes away, and the field with it. Envelopes carry
  `sender` and an explicit recipient list; a booth that has only "the connection that opened this
  view" addresses that one device and names no grouping at all.
- Creation of a channel is not a venue verb. No id is issued here, no membership is stored here.
- An application that needs a group implements it in its booth, in the booth's storage, with its
  own rules — and can change those rules without touching the transport.
- The 液态化 shape needs none of this: the renderer opens a view on its connection, the
  application answers on that connection, and there is nothing to be a member of.
- Read literally, today's wire is wrong in a way that fails quietly: a booth obeying this ADR and
  emitting `channel_id: 7` reaches no renderer, because 7 is compared against device ids. That
  is the defect this ADR removes rather than documents.

## Update — 2026-10-31: the word leaves the wire it was read from

The ADR ruled `channel_id` away and the sending half followed at once (gravity's turn emits
`{name, device, data}` with no sender of its own); the RECEIVING half kept the old spelling alive:
`forward` looked for `channel_id` first and fell back to `device`. Both readings are gone — the
downlink addresses `device` and nothing else — so a booth that emits the retired field now reaches
no renderer instead of reaching one by luck. The silent-failure defect this ADR was written about
("a booth emitting `channel_id: 7` reaches no renderer") is now the ONLY behaviour, which is what
makes it safe to say the field is gone rather than deprecated.
