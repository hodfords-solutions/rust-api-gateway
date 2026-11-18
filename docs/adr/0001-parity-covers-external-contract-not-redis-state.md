---
status: accepted
---

# Parity covers the External Contract, not Redis state

The port must let existing NestJS upstream services and existing clients keep working unchanged, so it reproduces the Reference Gateway's External Contract exactly (forwarded headers, error statuses and bodies, the Extension Contract, served document and MCP endpoints). It does not promise to share Redis keys, value formats or Lua scripts with the Reference Gateway, because that would freeze the rate-limit storage design to the Node implementation for no benefit to clients or upstreams.

## Considered Options

- **Full drop-in, including Redis state**: would allow both gateways to run behind one load balancer with shared rate-limit counters during a gradual rollout. Rejected: nobody needs mixed fleets today, and it locks the storage layout.
- **Feature-equivalent only**: free to redesign responses and behaviours. Rejected: upstream services and clients would have to change.

## Consequences

- A rollout that mixes both gateways behind one load balancer counts rate limits separately per implementation; cut over per environment instead.
- `libs/client` is not ported. Upstream services keep using the TypeScript package, and what it emits is treated as a fixed input format covered by tests.
