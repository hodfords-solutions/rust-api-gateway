# Fuzzing

`fuzz/` holds [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer) targets for
the code in `api-gateway` that reads untrusted input: request targets, headers and raw
connection bytes from clients, OpenAPI documents and WebSocket responses from Upstream
Services, and MCP tool arguments. Each target checks properties beyond "does not panic"
where the code has one worth stating.

Findings are documented here, not fixed: the crate is a port of the Reference Gateway, and
any change of behaviour is a Deviation that needs its own approval (see `docs/parity/`).

## Setup

Fuzzing needs a nightly toolchain (the workspace itself builds on stable) and cargo-fuzz:

```bash
rustup toolchain install nightly --profile minimal
```

```bash
cargo install cargo-fuzz --locked
```

`fuzz/` is its own Cargo workspace (the root `Cargo.toml` excludes it) with its own lock file.
cargo-fuzz builds with `--cfg fuzzing`, which compiles `api_gateway::fuzzing`
(`crates/rust-gateway/src/fuzzing.rs`): thin wrappers around the crate-private parsers the
targets need. It does not exist in normal builds and is not public API.

## Running

Seed the corpora from the golden data first (deterministic; standard-library Python):

```bash
python3 fuzz/scripts/seed-corpora.py
```

Then, from `fuzz/`, run one target:

```bash
cargo +nightly fuzz run request_url -- -max_total_time=300 -max_len=4096 -timeout=10 -rss_limit_mb=2048
```

or every target in turn (60 seconds each by default; logs in `fuzz/logs/`; it also sets
`ASAN_OPTIONS`, see "Observations" below):

```bash
fuzz/scripts/run-all.sh 120
```

The limits are part of the checks: `-timeout=10` turns runaway backtracking or quadratic
work into a reported timeout, and `-rss_limit_mb=2048` (libFuzzer's malloc limit follows it)
turns unbounded allocation into a reported out-of-memory. `run-all.sh` uses `-max_len` 64 KiB
for the document targets, 16 KiB for the byte-stream targets and 4 KiB for the rest.

Build every target without running:

```bash
cd fuzz && cargo +nightly fuzz build
```

## Targets

Text inputs are read as lossy UTF-8. Fields are separated as each target's header comment
says; the seeds in `fuzz/corpus/<target>/seed-*` show the format.

| Target | Code under test | Input | Checks besides "no panic" |
| --- | --- | --- | --- |
| `route_pattern` | `route::PathPattern` (`path-to-regexp` 8 port: tokenizer, group expansion, backtracking matcher) | patterns (`\x1f`-separated) `\0` paths (`\0`-separated) | `is_match` and `matches` agree; a match reports the whole input, only pattern keys, each once |
| `route_round_trip` | `route::PathPattern` | structured (`arbitrary`): literal segments, `:params` with values, a trailing `*wildcard` | the path built by substituting percent-encoded values matches, and the parameters decode back to exactly those values |
| `request_url` | `request::normalize_request_url`, `malformed_path_segment`, `classify`, `strip_service_prefix`, `upstream_target`; `route::uri::path_from_url`, `decode_uri_component` | target `\0` Service Prefix `\0` bypass prefixes | no dot segment (`.`, `..`, `%2e` forms) survives normalisation, the Route-lookup path, or the upstream target; the upstream target is origin-form; the two `decodeURIComponent` ports agree |
| `request_query` | `request::forwarded_query`, `parse_query`, `query_escape`, `query_unescape`, `kebab_case`; `documents::document_type` | the query | re-serialisation is a fixed point; `escape` emits only unreserved characters and `%XX`; `unescape(escape(s)) == s` |
| `request_headers` | `request::node_incoming_headers`, `upstream_request_headers`, `response_headers`, `strip_excluded_headers`; `rate_limit::forwarded_for` | lines: method, peer, `name:value`, `@middleware:value`, `-excluded` | no hop-by-hop header is forwarded; one `x-forwarded-for`, ending with the peer; response headers keep every non-hop-by-hop line; excluded headers are gone |
| `openapi_document` | `openapi::DocumentStore::apply` (Route table build), `RouteTable::lookup`, `camelcase_keys`, `convert_to_express_path` | document JSON `\0` request URLs (`\n`-separated) | — |
| `documents_json` | `documents::transform_document` and its `JSON.parse`/`JSON.stringify` port | the upstream body | the JSON round trip is a fixed point; a transformed document is again a document |
| `client_address` | `rate_limit::TrustedProxies` (`proxy-addr`/`ipaddr.js` port), `normalize_ip` | trust `\0` peer `\0` x-forwarded-for `\0` address `\0` IPv6 subnet | the client address is the peer or a forwarded entry; trusting nothing gives the peer; `normalize_ip` is idempotent |
| `inbound_stream` | `server::inbound::Inbound` (request-target rewrite on raw connection bytes, before hyper) | control byte, then client bytes | output does not depend on how the bytes are split into reads; no request head that hyper would act on in the output has a byte above `0x7e` in its target (a framing disagreement with hyper would let an unrewritten head through) |
| `upgrade_head` | `server::upgrade::UpgradeHead::parse`, `rewrite_target` | a request head, or a bare target | a recognised upgrade head is complete, printable, has `Upgrade` and `Connection: upgrade`, and is not `CONNECT`; a rewritten target has no byte above `0x7e` and is accepted by the `http` crate |
| `websocket_response` | `proxy::websocket` upstream response head parser and body decoder | response head, then the following bytes | the refusal status line is one line; the decoded body is no longer than its encoding |
| `mcp_tools` | `mcp::build_tools` (tool names, input schemas, `$ref` resolution) | document JSON | — |
| `mcp_call` | `mcp::build_upstream_call` | a case shaped like `src/mcp/testdata/call.json` | (Deviation D7) an argument adds no path segment, no `?` or `#`, no dot segment, and leaves the template before its first placeholder unchanged |

## From a crash to a regression test

1. Reproduce: `cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<file>`.
2. Minimise: `cargo +nightly fuzz tmin <target> fuzz/artifacts/<target>/<file>` (for a
   timeout, add `-- -timeout=2`).
3. Keep the minimised input as `fuzz/regressions/<target>/FUZZ-NN-<short-name>` and give it
   an ID in the findings table below. Re-run it with
   `cargo +nightly fuzz run <target> fuzz/regressions/<target>/FUZZ-NN-<short-name>`.
4. Write the input as a unit test next to the code (the crate's existing `tests` modules),
   asserting the behaviour that should hold. While the finding is open, mark the test
   `#[ignore = "fuzz finding FUZZ-NN: ..."]`; run open findings with
   `cargo test -p api-gateway -- --ignored fuzz`.
5. Fixing a finding changes behaviour: decide it as a Deviation (or confirm the Reference
   Gateway does the same), then remove the `#[ignore]` and move the row to "fixed".

`fuzz/regressions/` is not a corpus: the CI job seeds from the golden data only. Targets that
would rediscover an open finding step around it when its ID is in `FUZZ_SKIP_OPEN_FINDINGS`
(see below), so CI keeps looking for new problems.

## Findings

Every finding below is in the port only: the Reference Gateway (Node 24, `path-to-regexp`
8.4.2) was checked with the same input and does not share it. None is a memory-safety
problem (the crate forbids `unsafe`); all are denial-of-service or parity issues. The
regression inputs are in `fuzz/regressions/<target>/`, and each has an ignored unit test:

```bash
cargo test -p api-gateway --all-features fuzz_0 -- --ignored
```

CI steps around these inputs through `FUZZ_SKIP_OPEN_FINDINGS` in
`.github/workflows/fuzz.yml` (see `api_gateway_fuzz::skip_open_finding`): each target skips
only the narrow input shape of the listed IDs. Local runs skip nothing unless the variable
is set. Remove an ID from the workflow when its finding is fixed.

| ID | Target | Input (regression file) | Violation | Severity | Reference Gateway | Unit test |
| --- | --- | --- | --- | --- | --- | --- |
| FUZZ-01 | `inbound_stream` | `GET /a HTTP/1.1\nHost: h\n\nGET /é HTTP/1.1\r\nHost: h\r\n\r\n` (`FUZZ-01-bare-lf-head`; fuzzer's own: `FUZZ-01-fuzzer-found`) | The inbound rewriter finds heads by `\r\n\r\n` only; hyper (`httparse`) also ends a head at a bare `\n\n`. Everything up to the next `\r\n\r\n` is taken as one head and passed on unrewritten, so hyper parses both requests and accepts the second one's raw non-ASCII target. | **Low.** A client can bypass the target rewrite on its own connection: requests Node refuses with 400 (bare-LF heads, raw bytes above `0x7e` in the target) are routed and forwarded, with the target percent-encoded by URL normalisation. No other client's traffic is affected and the gateway re-serialises every forwarded request, so it is not request smuggling towards the upstream. | Not shared: Node answers 400 to a bare-LF head. | `server::inbound::tests::fuzz_01_a_bare_lf_head_does_not_hide_the_next_head` |
| FUZZ-02 | `websocket_response` | `HTTP/1.1 204 \n\r\n` (`FUZZ-02-bare-lf-status-line`) | When an upstream refuses a WebSocket upgrade, the reason phrase is taken up to the first CRLF. A status line ended by a bare LF takes the following header line into the reason, and the refusal status line written to the client spans two lines (`HTTP/1.1 401 Nope\nX-Injected: 1`). | **Low.** Only an Upstream Service can trigger it, and it already controls the headers forwarded in the refusal; the effect is a malformed or duplicated header line in the client's response. | Not shared: Node's client rejects the head (`HPE_CR_EXPECTED`). | `proxy::websocket::tests::fuzz_02_the_refusal_line_is_one_line` |
| FUZZ-03 | `route_pattern` | pattern `/*w0/*w1/…/*w23`, path `/x/x/…` with 23 segments (`FUZZ-03-24-wildcards`; reproduce with `-timeout=2`) | Each `*wildcard` after the first compiles to two identical alternatives (`([^\/]+\|[^\/]+)`) and the matcher retries every repetition length, so a failing match is exponential in the number of wildcards: 24 wildcards take 0.18 s in a release build, and each extra one doubles it. | **Low.** Needs a route with 20 or more wildcard segments in an upstream document; then any client request with that method and a near-miss path blocks a runtime worker thread for seconds to minutes, synchronously. Found by reviewing the matcher; the fuzzer did not reach 20+ wildcards in 2 × 150 s, the harness reproduces it. | Not shared: V8 answers the same regular expression in under 1 ms. | `route::pattern::tests::fuzz_03_many_wildcards_fail_fast` |
| FUZZ-04 | `mcp_tools` | a 2.3 KB document: a request body `$ref` to `S0`, each `S<i>` with two properties referencing `S<i+1>`, 16 levels (`FUZZ-04-ref-chain-16`; reproduce with `-timeout=2`) | Deviation D8 resolves a schema every time it is referenced, so a chain where each level references the next twice expands to 2^n copies: 12 levels already give a 242 KB input schema, 16 levels take over a minute under the fuzzer's instrumentation, and around 24 levels (about 3 KB of document) would need gigabytes. | **Medium.** A malicious or compromised Upstream Service document makes every MCP tool refresh (every 5 minutes and after each document change) spend exponential CPU and memory, which can exhaust the gateway process for all clients, and serves an enormous `tools/list`. Found by reviewing D8 while writing the target; the harness reproduces it. | Not shared: the reference resolves each reference once per request body (the behaviour D8 deviates from). A fix needs a decision on D8, for example a depth or size budget. | `mcp::tests::fuzz_04_shared_references_do_not_expand_exponentially` |
| FUZZ-05 | `inbound_stream` | `\r\n\r\nT /ҝ HTTP/1.0\r\n\r\n` after control byte `0x07` (`FUZZ-05-leading-empty-lines`) | Empty lines before the first request line are taken as a head of their own; it has no request line, so the rewriter marks the rest of the connection opaque. hyper skips the empty lines and parses every later head with its raw target. | **Low.** Same effect as FUZZ-01, from a different cause: a client that starts its connection with `\r\n\r\n` turns the target rewrite off for that connection. | Not shared: Node skips the empty lines and answers 400 to the non-ASCII target. | `server::inbound::tests::fuzz_05_leading_empty_lines_do_not_stop_the_rewrite` |

### Observations that are not findings

- `route_pattern` reached about 2 to 3 GB of RSS in long runs. With a smaller AddressSanitizer
  quarantine and shorter allocation stacks it stays near 350 MB, and no single input
  allocated more than 256 MB (`-malloc_limit_mb=256`): the growth is the sanitizer's
  bookkeeping for the matcher's deep, varied recursion. `run-all.sh` sets `ASAN_OPTIONS`
  accordingly.
- The `inbound_stream` property only checks heads hyper would act on. It does not check
  that body bytes pass through unchanged when the rewriter and hyper disagree about
  framing; that needs a byte-accurate model of hyper's body reading.

## Run log

Local runs on 2026-11-18, macOS arm64 (10 cores, up to 7 targets in parallel),
`cargo-fuzz` 0.13 with nightly Rust and AddressSanitizer, seeds from
`scripts/seed-corpora.py`, `-timeout=10 -rss_limit_mb=2048`. "cov" and "ft" are
libFuzzer's coverage and feature counts at the end of the run.

| Target | Run | Executions | exec/s | cov | ft | Result |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| `route_pattern` | 150 s | 652,749 | 4,322 | 1,365 | 7,969 | clean |
| `route_pattern` | 150 s, `-len_control=0 -max_len=2048` | 875,720 | 5,799 | 1,365 | 8,234 | clean |
| `route_round_trip` | 150 s | 1,578,245 | 10,451 | 950 | 4,343 | clean |
| `request_url` | 150 s | 829,104 | 5,490 | 3,883 | 14,625 | clean |
| `request_query` | 150 s | 441,908 | 2,926 | 1,071 | 5,465 | clean |
| `request_headers` | 150 s | 1,260,784 | 8,349 | 3,325 | 13,367 | clean |
| `openapi_document` | 150 s | 366,362 | 2,426 | 9,594 | 30,528 | clean |
| `documents_json` | 150 s | 621,401 | 4,115 | 1,193 | 4,689 | clean |
| `client_address` | 150 s | 1,421,584 | 9,414 | 4,313 | 12,529 | clean |
| `inbound_stream` | first run | 156,809 | — | 1,825 | 4,424 | FUZZ-01 after about 11 s |
| `inbound_stream` | FUZZ-01 skipped | 786,165 | — | 2,110 | 7,558 | FUZZ-05 |
| `inbound_stream` | 150 s, FUZZ-01 and FUZZ-05 skipped | 1,324,142 | 8,769 | 2,222 | 9,092 | clean |
| `upgrade_head` | 150 s | 2,638,305 | 17,472 | 1,320 | 5,727 | clean |
| `websocket_response` | first run | 111 | — | 1,138 | 1,479 | FUZZ-02 at once |
| `websocket_response` | 150 s, FUZZ-02 skipped | 1,893,245 | 12,538 | 2,147 | 7,356 | clean |
| `mcp_tools` | 150 s | 613,066 | 4,060 | 8,983 | 28,076 | clean (FUZZ-04 found by review) |
| `mcp_tools` | 150 s, second run | 763,948 | 5,059 | 9,214 | 29,446 | clean |
| `mcp_call` | 150 s | 2,448,098 | 16,212 | 3,020 | 12,631 | clean |
