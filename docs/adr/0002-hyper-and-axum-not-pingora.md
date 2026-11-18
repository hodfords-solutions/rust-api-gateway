---
status: accepted
---

# The gateway is built on hyper and axum, not Pingora

The gateway's HTTP layer is tokio + hyper 1.x + tower, exposed as an axum router. Cloudflare Pingora was the alternative, and we agreed beforehand to pick it only if a spike showed it passing three feasibility checks (reference-style WebSocket handling, JSON error bodies, `POST /mcp` with `rmcp`) and beating a hyper proxy by at least 20% in requests per second or p99 latency. Pingora passed the checks but was slower at every measured point, so the rule selected hyper.

## Considered Options

- **Pingora 0.9**: passed all three checks, but only after overriding three defaults that each surfaced as a failing check (`HttpUpstreamRequestPolicy::preserve()`, a unique pool key so upgrades never reuse a pooled connection, no read timeout on upgraded sessions). On the spike workload it handled 21–33% fewer requests per second than hyper with a higher p99, needed about 1.8 times the glue code, and brought 89 more crates. Its strengths (load balancing, HTTP/2, TLS termination, zero-downtime upgrade) are features the Reference Gateway does not have.
- **hyper + axum**: one handler for HTTP and one for WebSocket, the embedding application's own routes and `rmcp` plug in as ordinary tower services, and the server starts and stops inside a test.

## Consequences

- The measurements were taken on a machine that other builds were using, so the margins are approximate; the direction was the same in all eight throughput comparisons and in an interleaved re-run. Numbers and method are in `spikes/http-stack/REPORT.md` on branch `claude/step1-http-spike-75ab1e`.
- Behaviours that sit below axum's handler model need a tower layer or a custom accept loop: closing a connection without any response (WS-06, WS-16) and the exact bytes of a refused upgrade (WS-15).
- Request-handling logic stays independent of the HTTP layer (`request`, `route`, `rate_limit` modules), so the choice can be revisited by rewriting only the server and forwarding code.
