# Development

## Prerequisites

- Rust stable (pinned by `rust-toolchain.toml`), edition 2024.
- For the Conformance Tests: Node 20.19 or later, pnpm, and Docker (for Redis).

## Unit tests and lints

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

The MCP server (area 08) is behind the cargo feature `mcp` of `api-gateway`. `rust-gateway-server` enables it, so workspace builds always include it; check the crate without it with:

```bash
cargo clippy -p api-gateway --all-targets -- -D warnings && cargo test -p api-gateway
```

OpenTelemetry tracing (area 09) is behind the cargo feature `otel` of `api-gateway`, also enabled by `rust-gateway-server` (its default feature `otel`; it starts tracing when `OTLP_URL` is set, with `SERVICE_NAME` as the service name). Check it alone with:

```bash
cargo clippy -p api-gateway --all-targets --features otel -- -D warnings && cargo test -p api-gateway --features otel
```

Its tests (`tests/otel.rs`) collect spans with an in-memory exporter and need no collector. To run the Conformance Tests against a server built without it, build with `cargo build -p rust-gateway-server --no-default-features`.

`cargo test` runs the unit tests (including the harness's own tests of the mock upstream and the raw client) and the Rust-only integration tests in `crates/rust-gateway/tests/`, which serve the gateway in-process. Those that need Redis (`rate_limit_redis_*.rs`, `gateway_rate_limit.rs`) find it like the harness does (`CONFORMANCE_REDIS_HOST`, `CONFORMANCE_REDIS_PORT`) and print `SKIP` when it is not reachable; `RATE_LIMIT_REQUIRE_REDIS=1` makes them fail instead, as CI does. The Conformance Tests are behind the cargo feature `conformance` because they need Node, Redis and a built reference app.

Fuzz targets for the parsers of untrusted input (nightly, cargo-fuzz) are in `fuzz/`; see [security/fuzzing.md](security/fuzzing.md).

The adversarial tests (`crates/rust-gateway/tests/security_*.rs`), their coverage and the security findings they record: [security/attack-tests.md](security/attack-tests.md).

## Conformance Tests

### One-time setup

```bash
docker compose -f conformance/docker-compose.yml up -d
```

```bash
cd conformance/reference && pnpm install --frozen-lockfile && pnpm build
```

Redis listens on `127.0.0.1:6399`. Override with `CONFORMANCE_REDIS_HOST` and `CONFORMANCE_REDIS_PORT`.

### Running the suite

Against the Reference Gateway (the default target):

```bash
cargo test -p conformance --features conformance
```

Against `rust-gateway-server` (build it first; scenarios whose IDs are not in `conformance/rust-implemented.txt` are skipped for this target and print `SKIP`):

```bash
cargo build -p rust-gateway-server && CONFORMANCE_TARGET=rust cargo test -p conformance --features conformance
```

Against both, also comparing their normalised observations:

```bash
cargo build -p rust-gateway-server && CONFORMANCE_TARGET=both cargo test -p conformance --features conformance
```

Useful variations:

- One area: append a filter, for example `-- forwarding` or `-- fwd_04`.
- See each scenario's observations: append `-- --nocapture`.
- `RUST_GATEWAY_SERVER_BIN=/path/to/binary` runs another build of the Rust target; `NODE=/path/to/node` picks the Node binary.

Each gateway start writes its configuration and output to `target/conformance/<scenario>-<target>-<n>.{json,log}`. Look there first when a target fails to start.

The full suite takes about 80 seconds against the reference; `errors::req_24_redis_unreachable` alone waits about 73 seconds for the reference's Redis retries.

### Running the pieces by hand

The reference app with a configuration file (format in [conformance/README.md](../conformance/README.md#configuration-file)):

```bash
cd conformance/reference && GATEWAY_CONFIG=/path/to/config.json pnpm start
```

The mock upstream, serving a fixture and printing each request it receives as a JSON line:

```bash
cargo run -p conformance --bin mock-upstream -- --fixture conformance/fixtures/users-service.json --port 5000
```

Regenerating the fixtures (only when a scenario needs a new shape; commit the output):

```bash
cd conformance/fixture-generator && pnpm install --frozen-lockfile && pnpm generate
```

## Benchmarks

Run by hand, never in CI. Details, the scenarios and how to read the numbers: [benches/README.md](../benches/README.md); results: [benches/RESULTS.md](../benches/RESULTS.md).

Micro-benchmarks of the request hot paths (criterion, about 7 minutes):

```bash
cargo bench -p api-gateway
```

End-to-end load test of `rust-gateway-server` against the Reference Gateway in Docker (needs Docker with 10 CPUs and Python 3). The full run takes about an hour and needs a quiet machine; the smoke run takes a few seconds per point and only proves the tooling works:

```bash
python3 benches/scripts/bench.py
```

```bash
python3 benches/scripts/bench.py --smoke
```

Both gateways run the benchmark middleware set EX-05 (JWT verification), which the user confirmed on 2026-11-18 as representative of their production middleware.

## Writing a scenario

Scenarios live in `conformance/harness/tests/conformance/`, one module per area. A scenario is a `#[tokio::test]` that calls `runner::run(name, covers, |target| async move { ... })`:

- `covers` lists the Parity Checklist IDs it proves.
- The closure starts its mock upstreams, starts the gateway with `target.start(config)`, sends requests with the raw client, asserts the reference's behaviour exactly, and returns `Observations`: the normalised values (no ports, dates or other run-specific data) that both targets must produce identically. In `both` mode the runner compares them.
- Each gateway start gets its own flushed Redis database, so scenarios can run in parallel.
- Handlers are driven by the test middleware protocol in [conformance/README.md](../conformance/README.md#test-middleware-protocol). If a scenario needs a handler behaviour the protocol lacks, extend the protocol in both targets and in that document.
- `runner::run_reference_only` is for checking the reference behaviour behind an approved Deviation. It never runs against the Rust target.

## Working rules

1. **Conformance Test first.** For each feature area, write the Conformance Test and see it pass against the `reference` target before writing Rust code for it.
2. **Then implement** in `api-gateway` until the scenario passes against `rust` (and `both`). Add the covered IDs to `conformance/rust-implemented.txt` in the same commit.
3. **Tick the row.** Every Parity Checklist row ends up naming its test in the Test column (`C (module::test_name)`, plus unit or Rust-only tests) and moves to `done`, `deviation` or `n/a` with a reason.
4. **Deviations are Rust-only tests.** A row under an approved Deviation (`docs/parity/README.md`) is tested with a Rust-only integration test of the Rust behaviour; the reference behaviour stays recorded in the row (and may be checked with `run_reference_only`).
5. **Checklist corrections are explicit.** When a test shows a row is wrong, fix the row in its own change and say so; do not rewrite rows silently.
6. One group of commits per area, with conventional-commit messages that quote the checklist IDs.

## Reference version

`conformance/reference/package.json` pins `@hodfords/nestjs-api-gateway` to an exact version with a committed `pnpm-lock.yaml`. Every Monday `.github/workflows/reference-bump.yml` bumps it to the latest npm release and opens a pull request; that pull request's conformance job shows whether the new release changed observable behaviour. Line numbers in the checklist refer to the version named in `docs/parity/README.md`.
