# 0018 — A device is a connection, a channel is business

Status: Accepted (2026-10-29). Corrects an earlier claim of this repository's design notes and of
the 2026-10-29 wire work, where `channel_id` was treated as a venue concept.

## Context

The connection plane (ADR-0017) assigns one **device** id per connection and binds it to a
**user** account. Both facts are verified here: the device is what the socket was given at
connect, the user is the registry's answer to `login`, and the auth declarations are enforced
against both before an event is delivered.

The wire also carried a field named `channel_id`, written as the device id (`ui.rs`, the venue's
own speech) and read back by comparing it to the receiving connection's device:

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

2. **A channel is a booth's concept, and a booth owns all of it.** A booth that implements groups
   holds the membership it needs in its own namespace, keyed however its business dictates, and
   answers "who is in this group" itself. prism does not know that a group exists, cannot
   enumerate one, and never validates membership. Drawn out on a conventional chat application,
   that ownership is: at view build the application asks its own store which channels the member
   is in, keeps the ACTIVE one and that channel's history, and answers the view in that state; a
   switch is the same answer with another channel active, and nothing about it reaches the venue.
   Its composer is a form under the messages whose ONLY field is what was said — WHO is speaking
   is the sender the venue stamped (ADR-0017 §7: prism mounts `sender {device, user}` into every
   page event, and the store's member id is the user's uuid), and WHICH conversation it lands in
   is the application's active channel, not a field a page could set to speak into somebody else's.
   Creation, invitations and the channel list are the application's storage and the application's
   rules; what the venue contributes is the device↔user binding and the addressing.

3. **`channel_id` is not a venue field, and the wire does not carry it in either direction.**
   Fan-out is an ADDRESS, not a membership test: an event that wants several recipients names them
   (device ids, or the users that resolve to them). "Send to everyone in channel 7" is a sentence
   only the booth that owns channel 7 can say, so it is the booth that turns a group into a
   recipient list. The sending half already speaks that way (gravity's turn emits
   `{name, device, data}` and no sender of its own) and the receiving half addresses `device` and
   nothing else — so a booth that emits the retired field reaches no renderer, instead of reaching
   one by luck. Addressing the field again would be a new decision, not a compatibility question.

4. **What the venue still guarantees,** and nothing more: a device id is the address of a live
   connection; an anchored id (`?device=`) survives a reconnect; the device↔user binding is the
   registry's; the auth declaration gates delivery before it happens; a sender is excluded from
   the fan-out of its own event.

5. **A model-first application has no channels at all,** and that is the shape this stack is
   built for. gravity's core is one person talking to a model; when a person needs another
   person, the MODEL speaks — it notifies, and coordination takes the shape 《液态化》 describes:
   no meeting is scheduled (`不约时间地点`), a proposal is pushed to whoever the topic's topology
   says is relevant, people answer or vote where they already are, and the system collects,
   revises and pushes again until it converges. A channel would be "会议改成群聊" — the same chain
   over another medium, which that document refuses in as many words and which gravity ADR-0006
   argues against at plane level (one maintained channel per pair, N(N-1)/2 of them, each one a
   semantic negotiation). Channel-shaped conversation can be added later as an application's
   choice; it is not a requirement of this plane.

6. **The STORE's `channel` is a container, not a product concept.** k10r keeps turns, members and
   sessions under a channel id — a namespace an application writes into. A deployment that serves
   one conversation names one and never shows it, which is also why a turn can name a channel and
   a person never does.

## Consequences

- The `channel_id` comparison in the downlink is gone, and the field with it. Envelopes carry
  `sender` and an explicit recipient list; a booth that has only "the connection that opened this
  view" addresses that one device and names no grouping at all.
- Creation of a channel is not a venue verb. No id is issued here, no membership is stored here.
- An application that needs a group implements it in its booth, in the booth's storage, with its
  own rules — and can change those rules without touching the transport.
- The 液态化 shape needs none of this: the renderer opens a view on its connection, the
  application answers on that connection, and there is nothing to be a member of.
- An application built on this plane cannot ask the venue "who is in channel 7". It has the
  device↔user binding, the recipient list it constructs itself, and nothing else.