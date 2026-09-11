# Prism

Entry point of the stateless agent architecture: WS gateway + CLI over WS. Design: [stateless-agent-architecture.md](../../.hermes/wiki/stateless-agent-architecture.md) (wiki), Aura (base engine).

Deliberately thin: auth, request parsing, turn submission into the Aura realm. Queues, retries, timeouts, cross-machine scheduling belong to Aura's realm model — no standalone queue component.

One protocol: the WS gateway carries turn submission and streaming replies; the CLI is a thin wrapper over the same WS protocol — no second RPC surface. Client connections pin to Prism (resident), never to Gravity (stateless, one turn per execution); scale-to-zero semantics are unaffected.
