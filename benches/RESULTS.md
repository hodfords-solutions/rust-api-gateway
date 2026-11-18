# Benchmark results

How these numbers are produced, and how to read them: [README.md](README.md).

**Assumption.** Both gateways run the benchmark middleware set EX-05: JWT bearer
verification in the gateway, `auth-user-id` set from the token's subject, and the Identity
resolved as the user id when authenticated and the client address otherwise. This is an
assumption about what production middleware does; nobody has confirmed it. Numbers for other
middleware may differ, in particular for the Reference Gateway, whose middleware runs as
JavaScript.

## Official results

Not run yet. They must be taken on a quiet machine (no builds, tests or other containers
running; check the load average first) with:

```bash
python3 benches/scripts/bench.py
```

It takes about an hour and replaces the section between the markers below.

<!-- BEGIN official -->
<!-- END official -->

## Smoke run (shared machine; not the official results)

**These numbers come from a smoke run: a few seconds per point, taken on a machine where
other sessions were compiling and running tests at the same time.** They show that the
tooling works end to end and that every correctness check passes. They say nothing reliable
about performance: do not quote them.

<!-- BEGIN smoke -->
Run `20261118T083254Z-smoke`, started 2026-11-18T08:33:00+00:00, finished 2026-11-18T08:41:18+00:00. Raw results: `benches/results/20261118T083254Z-smoke/`.

- **Machine:** Apple M1 Pro (10 CPUs, 32 GiB), macOS 26.3 (Darwin 25.3.0)
- **Docker:** 29.2.1 (linux/arm64); VM with 10 CPUs and 7.7 GiB (Docker Desktop, kernel 6.12.69-linuxkit)
- **Gateways:** rust-gateway-server 0.0.0 at 563fd9aa4a6a (built with rustc 1.100.0 (0ef1bc386 2026-11-09)); @hodfords/nestjs-api-gateway 12.4.2 on Node v24.20.0, one process
- **Tools:** oha 1.17.0; Redis server v=7.4.12 sha=00000000:0 malloc=jemalloc-5.3.0 bits=64 build=6d565d4e3893fa59
- **Commit:** `563fd9aa4a6a58ad5eb2119f9abe883d6aced62a` (with uncommitted changes)
- **Load average (1, 5, 15 min):** host [12.75, 11.37, 9.27] before, [6.97, 7.38, 8.04] after; Docker VM ['2.13', '2.48', '2.40'] before, ['4.26', '3.80', '3.09'] after  **(busy machine: numbers are not comparable to a quiet run)**
- **CPU sets:** gateway 0 (1 CPU), 0-3 (4 CPU), memory 1g, the same for both gateways; upstream 4-5; Redis 6; oha 7-9. The gateway not being measured is paused.
- **Load:** 2 runs of 3 s per gateway per point, after 1 s of warm-up, alternating between the gateways (ABBA); keep-alive connections: 64, 256.
- **Middleware (EX-05):** JWT verification (HS256) in the gateway, `auth-user-id` from the token's subject, Identity = user id or client address. *This is an assumption about production middleware that has not been confirmed.*

Columns: req/s, p50 and p99 are medians over the runs (each run's req/s and p99 follow); *checked* = responses that all had the expected status and matched the upstream's count; peak RSS = `VmHWM` of the gateway process over the whole point (warm-up and all runs); CPU = `docker stats` halfway through each run, gateway · upstream · Redis.

#### Mock upstream measured directly (its ceiling)

| scenario | conc | req/s (median) | p99 ms | req/s of each run |
|---|---|---|---|---|
| get_small | 64 | 285,027 | 0.47 | 284,938, 285,116 |
| get_small | 256 | 285,161 | 1.56 | 284,184, 286,137 |
| post_json | 64 | 218,504 | 0.70 | 223,999, 213,009 |
| post_json | 256 | 214,256 | 3.52 | 223,193, 205,319 |
| get_large | 64 | 8,988 | 13.35 | 8,972, 9,005 |
| get_large | 256 | 7,193 | 83.24 | 6,746, 7,640 |
| static | 64 | 203,497 | 0.78 | 197,643, 209,352 |
| static | 256 | 212,688 | 2.38 | 206,809, 218,567 |

#### 1 CPU per gateway (CPU set 0)

| scenario | conc | gateway | req/s | p50 ms | p99 ms | checked | non-expected | errors | peak RSS | req/s of each run | p99 ms of each run | CPU % |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **get_small** | 64 | Rust | 13,635 | 4.52 | 7.53 | 81,960 | 0 | 0 | 20.9 MiB | 13,575, 13,695 | 7.33, 7.72 | 100.76%/101.10% · 17.41%/17.53% · 29.64%/29.40% |
| **get_small** | 64 | Reference | 3,522 | 15.11 | 62.78 | 21,380 | 0 | 0 | 194.5 MiB | 3,013, 4,030 | 72.24, 53.32 | 100.33%/74.59% · 2.47%/3.45% · 6.49%/11.65% |
| **get_small** | 256 | Rust | 14,831 | 16.98 | 24.21 | 89,512 | 0 | 0 | 29.3 MiB | 14,660, 15,002 | 26.93, 21.49 | 100.65%/75.36% · 16.73%/12.43% · 9.70%/9.87% |
| **get_small** | 256 | Reference | 3,324 | 55.36 | 839.29 | 20,307 | 0 | 0 | 223.9 MiB | 2,896, 3,752 | 1,007.17, 671.41 | 82.30%/73.90% · 2.58%/3.23% · 5.91%/10.13% |
| post_json | 64 | Rust | 12,296 | 4.97 | 9.98 | 73,906 | 0 | 0 | 20.5 MiB | 12,246, 12,347 | 10.74, 9.23 | 73.25%/71.51% · 16.88%/16.35% · 30.47%/29.92% |
| post_json | 64 | Reference | 3,142 | 16.97 | 62.06 | 19,110 | 0 | 0 | 205.4 MiB | 2,902, 3,381 | 65.62, 58.49 | 80.16%/100.64% · 6.39%/7.29% · 8.00%/12.18% |
| post_json | 256 | Rust | 12,658 | 19.37 | 34.70 | 76,408 | 0 | 0 | 31.7 MiB | 12,072, 13,245 | 45.26, 24.15 | 73.52%/74.40% · 21.59%/15.62% · 17.34%/13.53% |
| post_json | 256 | Reference | 3,057 | 64.40 | 794.09 | 18,708 | 0 | 0 | 202.5 MiB | 2,777, 3,338 | 834.68, 753.50 | 100.43%/100.64% · 4.90%/7.08% · 7.52%/10.76% |
| get_large | 64 | Rust | 2,394 | 26.22 | 46.51 | 14,469 | 0 | 0 | 61.3 MiB | 2,374, 2,413 | 50.42, 42.61 | 73.05%/78.03% · 20.76%/23.44% · 4.80%/6.70% |
| get_large | 64 | Reference | 508 | 110.75 | 690.73 | 3,162 | 0 | 0 | 274.8 MiB | 474, 543 | 833.16, 548.30 | 82.45%/84.89% · 5.95%/8.05% · 1.72%/1.92% |
| get_large | 256 | Rust | 2,233 | 111.05 | 177.25 | 13,860 | 0 | 0 | 103.2 MiB | 2,248, 2,218 | 157.63, 196.88 | 82.08%/99.59% · 23.79%/30.25% · 2.70%/2.66% |
| get_large | 256 | Reference | 508 | 197.68 | 3,517.33 | 3,626 | 0 | 0 | 296.7 MiB | 480, 536 | 3,540.54, 3,494.11 | 100.71%/100.49% · 6.72%/7.83% · 1.98%/1.95% |
| reject_401 | 64 | Rust | 19,656 | 3.08 | 5.41 | 118,081 | 0 | 0 | 17.0 MiB | 19,470, 19,841 | 5.67, 5.15 | 101.11%/100.58% · 0.00%/0.01% · 37.72%/37.62% |
| reject_401 | 64 | Reference | 6,252 | 9.26 | 24.38 | 37,615 | 0 | 0 | 155.3 MiB | 5,782, 6,723 | 34.30, 14.46 | 78.27%/75.62% · 0.00%/0.00% · 14.90%/16.95% |
| reject_401 | 256 | Rust | 22,643 | 10.98 | 17.68 | 136,322 | 0 | 0 | 25.2 MiB | 22,695, 22,592 | 17.77, 17.60 | 100.87%/100.46% · 0.01%/0.00% · 14.07%/9.90% |
| reject_401 | 256 | Reference | 6,179 | 35.63 | 194.30 | 37,392 | 0 | 0 | 162.1 MiB | 5,664, 6,694 | 339.46, 49.15 | 76.17%/73.73% · 0.00%/0.00% · 14.42%/12.62% |
| static | 64 | Rust | 25,615 | 2.32 | 4.17 | 153,928 | 0 | 0 | 19.5 MiB | 25,576, 25,653 | 4.29, 4.06 | 72.99%/70.90% · 24.59%/23.43% · 0.31%/0.33% |
| static | 64 | Reference | 6,172 | 6.81 | 45.18 | 37,362 | 0 | 0 | 221.8 MiB | 5,579, 6,766 | 48.32, 42.05 | 75.31%/100.80% · 5.71%/7.90% · 0.33%/0.33% |
| static | 256 | Rust | 24,201 | 10.39 | 15.02 | 145,840 | 0 | 0 | 27.6 MiB | 23,983, 24,419 | 15.81, 14.24 | 76.13%/101.13% · 20.37%/20.38% · 0.33%/0.33% |
| static | 256 | Reference | 5,411 | 32.24 | 432.43 | 32,797 | 0 | 0 | 197.5 MiB | 4,786, 6,036 | 768.85, 96.01 | 100.34%/100.60% · 5.23%/7.49% · 0.33%/0.34% |

#### 4 CPUs per gateway (CPU set 0-3)

| scenario | conc | gateway | req/s | p50 ms | p99 ms | checked | non-expected | errors | peak RSS | req/s of each run | p99 ms of each run | CPU % |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **get_small** | 64 | Rust | 33,705 | 1.87 | 3.02 | 202,525 | 0 | 0 | 23.1 MiB | 34,162, 33,248 | 2.86, 3.18 | 272.30%/385.27% · 51.58%/35.96% · 26.38%/26.53% |
| **get_small** | 64 | Reference | 4,965 | 12.19 | 21.54 | 29,935 | 0 | 0 | 223.2 MiB | 4,810, 5,119 | 23.69, 19.38 | 92.81%/93.25% · 4.18%/4.14% · 12.53%/12.90% |
| **get_small** | 256 | Rust | 33,240 | 7.57 | 11.80 | 200,052 | 0 | 0 | 35.5 MiB | 33,228, 33,253 | 12.00, 11.60 | 271.13%/276.58% · 35.86%/36.57% · 25.41%/25.05% |
| **get_small** | 256 | Reference | 4,518 | 49.26 | 376.32 | 27,426 | 0 | 0 | 227.4 MiB | 4,202, 4,834 | 683.56, 69.07 | 150.47%/95.09% · 5.49%/4.15% · 12.02%/12.43% |
| post_json | 64 | Rust | 26,619 | 2.35 | 4.08 | 159,988 | 0 | 0 | 23.3 MiB | 25,968, 27,271 | 4.42, 3.75 | 259.16%/256.10% · 42.01%/41.36% · 29.26%/28.45% |
| post_json | 64 | Reference | 4,361 | 14.03 | 23.38 | 26,248 | 0 | 0 | 194.5 MiB | 4,115, 4,607 | 26.87, 19.89 | 154.79%/128.50% · 8.54%/6.18% · 14.40%/10.62% |
| post_json | 256 | Rust | 25,409 | 9.75 | 20.46 | 153,043 | 0 | 0 | 39.4 MiB | 25,325, 25,494 | 21.51, 19.40 | 256.02%/369.72% · 39.93%/58.63% · 30.05%/30.13% |
| post_json | 256 | Reference | 4,123 | 55.24 | 360.14 | 25,075 | 0 | 0 | 205.5 MiB | 3,821, 4,425 | 502.46, 217.82 | 102.67%/128.87% · 6.64%/6.13% · 13.54%/9.76% |
| get_large | 64 | Rust | 7,263 | 8.76 | 19.15 | 43,771 | 0 | 0 | 67.4 MiB | 7,242, 7,284 | 18.60, 19.71 | 362.48%/381.16% · 111.75%/120.12% · 27.53%/29.27% |
| get_large | 64 | Reference | 807 | 75.97 | 214.44 | 4,962 | 0 | 0 | 279.6 MiB | 785, 830 | 240.23, 188.66 | 131.29%/130.16% · 11.59%/11.11% · 3.41%/2.50% |
| get_large | 256 | Rust | 6,391 | 37.78 | 112.62 | 38,844 | 0 | 0 | 130.9 MiB | 6,547, 6,234 | 78.62, 146.63 | 382.11%/294.34% · 116.46%/89.45% · 18.09%/14.21% |
| get_large | 256 | Reference | 810 | 236.86 | 2,982.36 | 5,337 | 0 | 0 | 306.6 MiB | 793, 828 | 2,712.21, 3,252.52 | 158.41%/158.00% · 11.19%/11.12% · 3.16%/2.71% |
| reject_401 | 64 | Rust | 45,386 | 1.37 | 2.69 | 272,766 | 0 | 0 | 21.2 MiB | 45,214, 45,558 | 2.65, 2.74 | 361.62%/246.14% · 0.00%/0.00% · 41.83%/28.58% |
| reject_401 | 64 | Reference | 6,426 | 9.41 | 17.48 | 38,664 | 0 | 0 | 162.9 MiB | 6,317, 6,535 | 18.84, 16.12 | 71.58%/95.63% · 0.00%/0.00% · 13.61%/17.35% |
| reject_401 | 256 | Rust | 47,888 | 5.28 | 8.13 | 287,999 | 0 | 0 | 29.2 MiB | 47,976, 47,800 | 8.14, 8.12 | 373.42%/272.89% · 0.00%/0.00% · 22.87%/33.06% |
| reject_401 | 256 | Reference | 6,346 | 36.92 | 67.77 | 38,464 | 0 | 0 | 165.4 MiB | 6,235, 6,458 | 89.99, 45.55 | 74.22%/102.26% · 0.00%/0.02% · 18.27%/18.03% |
| static | 64 | Rust | 57,159 | 1.09 | 1.96 | 343,432 | 0 | 0 | 23.9 MiB | 57,085, 57,232 | 1.97, 1.95 | 266.58%/269.58% · 62.62%/63.42% · 0.35%/0.35% |
| static | 64 | Reference | 8,517 | 6.66 | 13.99 | 51,268 | 0 | 0 | 197.0 MiB | 8,259, 8,776 | 13.48, 14.51 | 107.95%/147.67% · 10.34%/10.30% · 0.29%/0.30% |
| static | 256 | Rust | 56,946 | 4.41 | 7.20 | 342,461 | 0 | 0 | 34.3 MiB | 56,934, 56,959 | 7.13, 7.27 | 267.47%/266.56% · 56.19%/56.16% · 0.35%/0.34% |
| static | 256 | Reference | 8,032 | 30.01 | 43.77 | 48,693 | 0 | 0 | 201.8 MiB | 7,953, 8,112 | 44.05, 43.49 | 117.58%/115.97% · 8.28%/7.88% · 0.33%/0.30% |

#### Rust compared with the Reference Gateway (medians)

| CPUs | scenario | conc | req/s Rust ÷ Reference | p99 Reference ÷ Rust | peak RSS Reference ÷ Rust |
|---|---|---|---|---|---|
| 1 | **get_small** | 64 | 3.87× | 8.34× | 9.30× |
| 1 | **get_small** | 256 | 4.46× | 34.67× | 7.63× |
| 1 | post_json | 64 | 3.91× | 6.22× | 10.01× |
| 1 | post_json | 256 | 4.14× | 22.88× | 6.39× |
| 1 | get_large | 64 | 4.71× | 14.85× | 4.48× |
| 1 | get_large | 256 | 4.40× | 19.84× | 2.87× |
| 1 | reject_401 | 64 | 3.14× | 4.51× | 9.12× |
| 1 | reject_401 | 256 | 3.66× | 10.99× | 6.43× |
| 1 | static | 64 | 4.15× | 10.83× | 11.38× |
| 1 | static | 256 | 4.47× | 28.78× | 7.15× |
| 4 | **get_small** | 64 | 6.79× | 7.12× | 9.65× |
| 4 | **get_small** | 256 | 7.36× | 31.88× | 6.41× |
| 4 | post_json | 64 | 6.10× | 5.73× | 8.33× |
| 4 | post_json | 256 | 6.16× | 17.60× | 5.22× |
| 4 | get_large | 64 | 9.00× | 11.20× | 4.15× |
| 4 | get_large | 256 | 7.89× | 26.48× | 2.34× |
| 4 | reject_401 | 64 | 7.06× | 6.49× | 7.67× |
| 4 | reject_401 | 256 | 7.55× | 8.34× | 5.66× |
| 4 | static | 64 | 6.71× | 7.14× | 8.24× |
| 4 | static | 256 | 7.09× | 6.08× | 5.88× |

Scenarios: `get_small`: GET of a Routed Request answered with a 110-byte JSON body, expects 200, upstream receives every request, with `auth-user-id`, Buckets: Global IP, Custom; `post_json`: POST of a Routed Request with a 4 KB JSON body; the Route has one rate limit, expects 201, upstream receives every request, with `auth-user-id`, Buckets: Global IP, Custom, Route; `get_large`: GET of a Routed Request answered with a 1 MiB body (streaming), expects 200, upstream receives every request, with `auth-user-id`, Buckets: Global IP, Custom; `reject_401`: A Routed Request whose token has a bad signature, rejected by the middleware, expects 401, upstream receives nothing, Buckets: Global IP, Anonymous IP; `static`: A Static Request (no Route, middleware or rate limit); its forged auth-user-id must be dropped, expects 200, upstream receives every request, without `auth-user-id`, Buckets: none.
<!-- END smoke -->

## Micro-benchmarks (smoke run on a shared machine)

Criterion results depend on the machine; compare a change before and after on the same
machine (`cargo bench -p rust-gateway -- --save-baseline before`, then `--baseline before`).
[README.md](README.md#micro-benchmarks) lists what each bench measures.

**One run of `cargo bench -p rust-gateway` (default criterion settings) on 2026-11-18,
15:41–15:48 (+07), on the machine above while other sessions were compiling and testing
(host load average 6.0 at the start, 3.9 at the end).** Absolute times are inflated by that
load; the proportions are what to look at:

- **Route lookup dominates.** A `GET` in the middle of the 282-operation document costs about
  13.5 µs of the 22.8 µs the whole composed pipeline costs; a miss scans all 142 `GET` Routes
  (26 µs). The lookup tries each Route's compiled pattern in document order (REQ-14), so its
  cost grows with the position of the Route and the size of the document.
- Next come the upstream header list (4.3 µs for 12 lines) and the middleware header's
  kebab-casing (0.9 µs for one header).
- Everything else per request is below 2 µs each.
- `router_rejected_by_middleware` (20 µs) is the real router up to a 401 from middleware,
  without rate limiting or forwarding.

| Bench | Median |
|---|---|
| `client_address/normalize_ip/v4` | 32.9 ns |
| `client_address/normalize_ip/v4_mapped_v6` | 116.9 ns |
| `client_address/normalize_ip/v6_subnet_64` | 536.5 ns |
| `client_address/request_state/no_trust_v4` | 159.1 ns |
| `client_address/request_state/no_trust_v6` | 743.8 ns |
| `client_address/request_state/sample_trust_direct_v4` | 157.8 ns |
| `client_address/request_state/sample_trust_two_hops` | 901.9 ns |
| `document_build/282_operations` | 3.41 ms |
| `headers/exclude_and_rebuild_12_lines` | 482.2 ns |
| `headers/response_headers_8_lines` | 427.9 ns |
| `headers/upstream_framing` | 3.2 ns |
| `headers/upstream_request_headers_12_lines` | 4.28 µs |
| `kebab/kebab_case/already_kebab` | 603.5 ns |
| `kebab/kebab_case/authUserId` | 486.4 ns |
| `kebab/proxy_request/add_one_then_kebab_headers` | 900.7 ns |
| `kebab/proxy_request/add_three_then_kebab_headers` | 2.50 µs |
| `pipeline/composed_routed_get` | 22.83 µs |
| `pipeline/router_rejected_by_middleware` | 20.25 µs |
| `rate_limit_in_memory/has_status_limits` | 1.0 ns |
| `rate_limit_in_memory/state_and_identity` | 190.6 ns |
| `route_lookup/get_first` | 537.3 ns |
| `route_lookup/get_last_health` | 14.80 µs |
| `route_lookup/get_middle_orders_id` | 13.54 µs |
| `route_lookup/get_middle_with_query` | 13.58 µs |
| `route_lookup/get_no_match` | 25.89 µs |
| `route_lookup/get_percent_encoded_param` | 15.26 µs |
| `route_lookup/post_middle_orders` | 3.16 µs |
| `service_selection/classify` | 116.4 ns |
| `service_selection/is_supported_method` | 2.2 ns |
| `service_selection/select/direct_prefix` | 64.5 ns |
| `service_selection/select/no_match` | 737.6 ns |
| `service_selection/select/service_prefix_4th_of_8` | 470.9 ns |
| `service_selection/strip_service_prefix` | 58.8 ns |
| `upstream_rewrite/forwarded_query` | 1.63 µs |
| `upstream_rewrite/target/routed` | 437.3 ns |
| `upstream_rewrite/target/routed_with_query` | 1.51 µs |
| `upstream_rewrite/target/static` | 379.0 ns |
| `url/malformed_check/percent_encoded` | 200.0 ns |
| `url/malformed_check/plain` | 178.2 ns |
| `url/normalize/dot_segments` | 382.3 ns |
| `url/normalize/percent_encoded` | 470.0 ns |
| `url/normalize/plain` | 343.6 ns |
| `url/normalize/with_query` | 530.0 ns |
