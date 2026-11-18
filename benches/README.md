# Benchmarks

Two kinds, both run by hand (never a CI gate):

- **Micro-benchmarks** ([criterion](https://docs.rs/criterion)) of the per-request hot paths
  in `crates/rust-gateway/benches/`, to see where a request's CPU time goes and to compare a
  change before and after.
- **End-to-end load test** of `rust-gateway-server` against the Reference Gateway, both in
  Docker in front of the same mock upstream and Redis, with the same configuration file and
  the same middleware. This is the headline comparison: requests per second, p50, p99 and
  memory.

Results and the machine they were taken on: [RESULTS.md](RESULTS.md).

**Middleware.** Both gateways run the middleware set EX-05 (below). The user confirmed on
2026-11-18 that it is representative of their production middleware.

## Micro-benchmarks

```bash
cargo bench -p api-gateway
```

About 7 minutes. One group only: `cargo bench -p api-gateway --bench hot_paths -- route_lookup`.
Compare a change: `cargo bench -p api-gateway -- --save-baseline before`, change the code,
then `cargo bench -p api-gateway -- --baseline before`.

Inputs are realistic: the Route table is built from `fixtures/bench-service.json`, the
282-operation document the load test also serves; headers are a browser's API request (12
lines, a bearer token) and a NestJS response (8 lines); eight Upstream Services are
configured. Each function is called the way `crates/rust-gateway/src/proxy/` calls it.

| Bench (`hot_paths`) | Pipeline step | Checklist |
|---|---|---|
| `route_lookup/*` | Route lookup: a linear scan of the method's Routes (142 for `GET`) in document order; first, middle, last, no match, with a query, with percent-encoding | REQ-14, OAS-19 |
| `document_build/282_operations` | Building the Route table (startup and each document refresh, not per request) | OAS-06 to OAS-17 |
| `url/normalize/*`, `url/malformed_check/*` | URL normalisation; Express's malformed-encoding check | REQ-02, REQ-26 |
| `service_selection/*` | Request class, Upstream Service selection among 8, Service Prefix removal, method check | REQ-04 to REQ-12, REQ-15 |
| `upstream_rewrite/*` | Upstream target (prefix removed, URL resolved, trailing slash) and the query as undici sends it | FWD-01 to FWD-05 |
| `headers/*` | Excluded headers, the upstream request header list, body framing, response headers | REQ-03, FWD-06 to FWD-15 |
| `kebab/*` | Kebab-casing middleware headers | REQ-21, REQ-22 |
| `client_address/*` | Client address under the trusted proxies, IP normalisation | RL-06 to RL-08 |
| `rate_limit_in_memory/*` | What a rate-limit check computes before Redis: request state, default Identity, status-limit check | RL-09 to RL-19 |

| Bench (`pipeline`) | What it measures |
|---|---|
| `pipeline/composed_routed_get` | Every pure step of one Routed Request (`GET /svc/v1/orders/42`), in pipeline order: the sum of the hot paths above as one request pays for them |
| `pipeline/router_rejected_by_middleware` | The real `Gateway::router()` handling one Routed Request in-process (axum, `node_compat`, the handlers) up to a middleware rejecting it with 401, and the error rendering; rate limiting off |

What cannot be measured without I/O today, and the seams that would allow it (neither
exists; adding them is a library change):

- **Forwarding.** `UpstreamPool` is private and always dials TCP. A builder method taking a
  `tower::Service<http::Request<Body>>` (or a connector) per Upstream Service would let a
  benchmark answer in memory and cover body wrapping, response headers and the pool.
- **Rate limiting.** `RateLimiter`'s in-memory store exists only under `cfg(test)`, and the
  Bucket keys are built by private functions (`rate_limit::bucket`). Exposing the memory store
  (for example `RateLimiter::in_memory` behind a `testing` feature) would let
  `router_rejected_by_middleware` keep rate limiting on and measure the key building.

## End-to-end load test

### Commands

Full run (about **an hour**; needs a **quiet machine**: no builds, tests or other containers
busy, check `uptime` first):

```bash
python3 benches/scripts/bench.py
```

Smoke run (a few seconds per point, about 9 minutes with images already built; proves the
tooling and the checks work, its numbers mean nothing):

```bash
python3 benches/scripts/bench.py --smoke
```

Needs Docker (with at least 10 CPUs for the default layout) and Python 3; nothing else on the
host. `--help` lists the options: `--configs`, `--concurrency`, `--runs`, `--duration`,
`--warmup`, `--scenarios`, the CPU sets, `--no-build`. The results go to
`benches/results/<UTC time>-<smoke|full>/` and the summary replaces the smoke or official
section of [RESULTS.md](RESULTS.md). Re-render a run's tables with
`python3 benches/scripts/summarize.py benches/results/<run>`.

Time of the full run: 2 CPU configurations × 5 scenarios × 2 concurrency levels = 20 points;
each point restarts both gateways (about 5 s), warms each up for 5 s and runs 3 × 20 s per
gateway, about 2 min 20 s; then 8 direct upstream measurements of 65 s each. About 55
minutes, plus image builds.

### What runs

All in the Compose project `rgw-bench` ([docker-compose.yml](docker-compose.yml)); the
script starts and removes only its own containers.

| Container | Image | CPU set (default) | Memory |
|---|---|---|---|
| `rust` | `rust-gateway-server` release build ([docker/rust-gateway.Dockerfile](docker/rust-gateway.Dockerfile)), `GATEWAY_MIDDLEWARE=bench` | `0` or `0-3` | 1 GiB |
| `reference` | NestJS app embedding `@hodfords/nestjs-api-gateway` 12.4.2 from npm, one Node process ([reference/](reference/)) | same as `rust` | 1 GiB |
| `upstream` | minimal hyper server ([upstream/](upstream/)) | `4-5`, 2 threads | 512 MiB |
| `redis` | `redis:7-alpine`, no persistence | `6` | 256 MiB |
| `oha` | `ghcr.io/hatoo/oha:1.17.0` | `7-9` | 1 GiB |

Two CPU configurations: **1 CPU** per gateway, and **4 CPUs** per gateway with the
Reference Gateway still one Node process, as it is deployed by default (the Rust gateway's
Tokio runtime uses every CPU it is given). Both gateways get the same CPU set and memory
limit; only one runs at a time, the other is paused (`docker pause`). The other containers
never share a core with the gateway. On macOS, Docker runs a Linux VM and these are that VM's
CPUs; containers of other projects in the same VM are not kept off them, which is one reason
the machine must be quiet.

Both gateways read the same configuration file, [fixtures/gateway-config.json](fixtures/gateway-config.json):
one Upstream Service `svc` whose document is [fixtures/bench-service.json](fixtures/bench-service.json)
(282 operations, generated by [fixtures/generate.py](fixtures/generate.py)), the sample's
excluded headers and trusted proxies, rate limiting **on** with every limit at 10⁹ per 60 s
(never reached, so each request makes the Reference Gateway's real number of Redis round
trips), documents enabled.

### Middleware (EX-05)

[`crates/rust-gateway-server/src/bench.rs`](../crates/rust-gateway-server/src/bench.rs) and
[`reference/src/middleware.ts`](reference/src/middleware.ts) implement the same rules:

- A request with `Authorization: Bearer <token>` must carry an HS256 JWT signed with
  `BENCH_JWT_SECRET`, unexpired, with a string `sub` (verified with `jsonwebtoken` in both:
  the npm package's `verify(token, secret, { algorithms: ['HS256'] })`, and the crate with
  the same options). `authUserId` (forwarded as `auth-user-id`) is set to `sub`. A bad token
  answers 401 `{"message":"Unauthorized","statusCode":401}`.
- No token: 401 when the Route requires bearer authentication, otherwise anonymous.
- Identity: the user id, authenticated, or else the client address, not authenticated.
- Static Requests (`/statics/`, `/images/`) and MCP authentication as the reference sample.

The script makes a fresh secret per run, a token valid for a week and a token with the same
claims signed with another secret (for the 401 scenario).

### Scenarios

One line each in [scenarios.jsonl](scenarios.jsonl); to add one, add a line (method, path,
headers with `${TOKEN}` or `${BAD_TOKEN}`, optional `body_file` from `fixtures/`, the
expected status, what the upstream must receive, and the Buckets it touches).

| Scenario | Request | Expected | Upstream receives | Buckets |
|---|---|---|---|---|
| **`get_small`** (headline) | `GET /svc/v1/orders/42`, valid token; 110-byte JSON answer | 200 | every request, with `auth-user-id` | Global IP, Custom |
| `post_json` | `POST /svc/v1/orders`, valid token, 4 KB JSON body; the Route has a rate limit | 201 | every request, with `auth-user-id` | Global IP, Custom, Route |
| `get_large` | `GET /svc/v1/reports/7/export`, valid token; 1 MiB answer (streaming) | 200 | every request, with `auth-user-id` | Global IP, Custom |
| `reject_401` | `GET /svc/v1/orders/42`, token with a bad signature | 401 | nothing | Global IP, Anonymous IP |
| `static` | `GET /svc/statics/app.css` with a forged `auth-user-id`; 2 KiB answer | 200 | every request, without `auth-user-id` | none |

### Method and checks

For each CPU configuration, scenario and concurrency level (64 and 256 keep-alive
connections), a **point**:

1. Both gateways are recreated (so peak memory is per point) and Redis is flushed.
2. **Pre-check:** one request to each gateway must get the expected status, and both
   gateways must answer the same body.
3. Each gateway is warmed up, then the measured runs alternate between the gateways in ABBA
   order, so background load hits both alike. oha waits for the requests in flight at the
   deadline (`-w`), so every request it sent is counted.
4. **Per run:** every response must have the expected status with no connection errors, and
   the upstream's counters (reset before the run) must equal the number of responses, with
   or without `auth-user-id` as the scenario says (0 for the 401 scenario). This proves every
   counted response really went through the gateway and its middleware.
5. Peak RSS (`VmHWM` of the gateway's PID 1) and the cgroup's `memory.peak` are read.

A point with any failed check is **not reported**: the summary shows why it was rejected, and
the script exits with status 1. Every run's numbers are reported, not only the medians, with
a `docker stats` CPU sample of the gateway, upstream and Redis halfway through each run, to
show which one is saturated.

The upstream is also measured **directly** with the same load generator for each forwarded
shape: its ceiling must be well above the gateways' numbers, or the run measured the
upstream. For the small bodies it is far above (about 200k–285k req/s in the smoke run). For
`get_large` it is not: 1 MiB answers through the Docker network top out near 9k req/s even
without a gateway, so a gateway that gets close to that (the Rust gateway at 4 CPUs did in the
smoke run) is limited by the network and the upstream, not by its own CPU. Read that
scenario's 4-CPU ratio as a lower bound.

The CPU column is one `docker stats` sample per run; on 3-second smoke runs it can land
outside the busy part of the run, so read it as a hint of which container is saturated, not
as a measurement.

Recorded with every run (`env.json`): machine, OS, Docker version and VM size, Rust and Node
versions, gateway versions, oha and Redis versions, commit hash (and whether the worktree had
uncommitted changes), the CPU layout, and the host and VM load averages before and after. A
host 1-minute load average above 1.0 at the start marks the run as taken on a busy machine.

### Files

| Path | What |
|---|---|
| `scripts/bench.py` | The run: builds, starts, measures, checks, writes `results/<run>/`, calls the summary |
| `scripts/summarize.py` | Turns `results/<run>/` into tables (`summary.md`, and the section of `RESULTS.md`) |
| `scenarios.jsonl` | The scenarios |
| `docker-compose.yml`, `docker/` | Images and containers |
| `reference/` | The Reference Gateway app (own `pnpm-lock.yaml`; `@hodfords/nestjs-api-gateway` at the version of `conformance/reference/`) |
| `upstream/` | The mock upstream (its own Cargo workspace and lockfile, built only in Docker) |
| `fixtures/` | The OpenAPI document and its generator, the POST body, the gateway configuration |
| `results/<run>/` | `env.json`, `scenarios.jsonl`, `raw/` (oha JSON per run with the upstream counters, checks and CPU samples; pre-checks; memory per point), `summary.md` |
