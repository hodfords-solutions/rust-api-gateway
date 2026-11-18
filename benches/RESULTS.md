# Benchmark results

How these numbers are produced, and how to read them: [README.md](README.md).

**Assumption.** Both gateways run the benchmark middleware set EX-05: JWT bearer
verification in the gateway, `auth-user-id` set from the token's subject, and the Identity
resolved as the user id when authenticated and the client address otherwise. The user
confirmed on 2026-11-18 that this is representative of their production middleware. Numbers for other
middleware may differ, in particular for the Reference Gateway, whose middleware runs as
JavaScript.

## Official results

The run below was taken on 2026-11-18 on a developer's desktop that was not idle (a browser,
an editor and system indexing were running), so treat the figures as indicative.

Part of it was disturbed more than that. From about 10:36 UTC another build was compiling on
the host. The points measured after that are the 4-CPU ones for `post_json` at 256
connections, `get_large`, `reject_401` and `static`; some of their runs spread widely (for
example Rust `reject_401` at 256 connections: 36,730, 40,293 and 20,289 req/s). All 1-CPU
points and the headline `get_small` at 4 CPUs were measured before it started.

To repeat it on a quiet machine (no builds, tests or other containers running; check the
load average first):

```bash
python3 benches/scripts/bench.py
```

It takes about an hour once the images are built, and replaces the section between the
markers below.

<!-- BEGIN official -->
Run `20261118T091045Z-full`, started 2026-11-18T09:55:21+00:00, finished 2026-11-18T10:51:33+00:00. Raw results: `benches/results/20261118T091045Z-full/`.

- **Machine:** Apple M1 Pro (10 CPUs, 32 GiB), macOS 26.3 (Darwin 25.3.0)
- **Docker:** 29.2.1 (linux/arm64); VM with 10 CPUs and 7.7 GiB (Docker Desktop, kernel 6.12.69-linuxkit)
- **Gateways:** rust-gateway-server 0.0.0 at 41b4f692f921 (built with rustc 1.100.0 (0ef1bc386 2026-11-09)); @hodfords/nestjs-api-gateway 12.4.2 on Node v24.20.0, one process
- **Tools:** oha 1.17.0; Redis server v=7.4.12 sha=00000000:0 malloc=jemalloc-5.3.0 bits=64 build=6d565d4e3893fa59
- **Commit:** `41b4f692f92100269e7b24308c1bddd600f10fa3`
- **Load average (1, 5, 15 min):** host [15.97, 17.86, 14.43] before, [5.32, 8.95, 9.32] after; Docker VM ['1.31', '0.54', '0.45'] before, ['3.65', '3.95', '3.75'] after  **(busy machine: numbers are not comparable to a quiet run)**
- **CPU sets:** gateway 0 (1 CPU), 0-3 (4 CPU), memory 1g, the same for both gateways; upstream 4-5; Redis 6; oha 7-9. The gateway not being measured is paused.
- **Load:** 3 runs of 20 s per gateway per point, after 5 s of warm-up, alternating between the gateways (ABBA); keep-alive connections: 64, 256.
- **Middleware (EX-05):** JWT verification (HS256) in the gateway, `auth-user-id` from the token's subject, Identity = user id or client address. Confirmed by the user on 2026-11-18 as representative of their production middleware.

Columns: req/s, p50 and p99 are medians over the runs (each run's req/s and p99 follow); *checked* = responses that all had the expected status and matched the upstream's count; peak RSS = `VmHWM` of the gateway process over the whole point (warm-up and all runs); CPU = `docker stats` halfway through each run, gateway · upstream · Redis.

#### Mock upstream measured directly (its ceiling)

| scenario | conc | req/s (median) | p99 ms | req/s of each run |
|---|---|---|---|---|
| get_small | 64 | 214,318 | 1.99 | 214,318, 205,345, 221,547 |
| get_small | 256 | 256,884 | 2.71 | 259,676, 236,215, 256,884 |
| post_json | 64 | 198,019 | 0.88 | 208,693, 188,377, 198,019 |
| post_json | 256 | 209,162 | 3.49 | 208,042, 211,663, 209,162 |
| get_large | 64 | 8,340 | 17.29 | 8,423, 8,340, 8,122 |
| get_large | 256 | 7,675 | 52.04 | 7,675, 7,654, 8,254 |
| static | 64 | 205,126 | 0.75 | 191,330, 205,126, 223,571 |
| static | 256 | 215,251 | 2.10 | 215,251, 209,357, 221,492 |

#### 1 CPU per gateway (CPU set 0)

| scenario | conc | gateway | req/s | p50 ms | p99 ms | checked | non-expected | errors | peak RSS | req/s of each run | p99 ms of each run | CPU % |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **get_small** | 64 | Rust | 13,182 | 4.72 | 6.96 | 791,277 | 0 | 0 | 23.0 MiB | 13,135, 13,234, 13,182 | 7.17, 6.75, 6.96 | 101.14%/101.50%/101.47% · 16.23%/16.38%/16.11% · 26.55%/26.43%/27.08% |
| **get_small** | 64 | Reference | 4,322 | 11.85 | 51.62 | 260,449 | 0 | 0 | 219.9 MiB | 4,322, 4,411, 4,282 | 51.62, 51.46, 54.04 | 100.83%/100.61%/100.38% · 4.78%/4.71%/4.53% · 10.57%/10.71%/10.51% |
| **get_small** | 256 | Rust | 13,991 | 18.00 | 25.00 | 811,992 | 0 | 0 | 33.4 MiB | 12,459, 13,991, 14,110 | 26.57, 25.00, 24.71 | 100.76%/100.55%/100.79% · 16.27%/15.62%/15.66% · 9.29%/9.19%/9.18% |
| **get_small** | 256 | Reference | 4,019 | 56.15 | 118.76 | 241,463 | 0 | 0 | 226.9 MiB | 3,930, 4,019, 4,099 | 119.60, 118.76, 110.22 | 100.43%/100.08%/100.45% · 4.33%/4.19%/4.41% · 9.88%/9.79%/9.88% |
| post_json | 64 | Rust | 12,082 | 5.23 | 7.28 | 724,856 | 0 | 0 | 24.2 MiB | 12,140, 12,010, 12,082 | 7.09, 7.34, 7.28 | 101.57%/101.09%/101.47% · 21.61%/21.81%/22.07% · 28.25%/28.33%/28.28% |
| post_json | 64 | Reference | 3,894 | 13.56 | 55.10 | 233,528 | 0 | 0 | 192.9 MiB | 3,875, 3,894, 3,895 | 54.55, 55.58, 55.10 | 102.58%/100.56%/100.44% · 7.28%/7.09%/7.02% · 11.90%/11.86%/12.06% |
| post_json | 256 | Rust | 12,495 | 20.33 | 25.77 | 749,562 | 0 | 0 | 37.1 MiB | 12,452, 12,495, 12,496 | 26.25, 25.77, 25.70 | 100.95%/103.01%/101.18% · 19.18%/19.65%/19.57% · 12.41%/12.54%/12.39% |
| post_json | 256 | Reference | 3,734 | 60.81 | 113.04 | 223,711 | 0 | 0 | 200.4 MiB | 3,671, 3,747, 3,734 | 114.60, 110.56, 113.04 | 100.66%/100.73%/100.34% · 7.07%/6.77%/6.99% · 10.70%/11.09%/10.78% |
| get_large | 64 | Rust | 2,437 | 26.23 | 36.81 | 146,325 | 0 | 0 | 62.6 MiB | 2,424, 2,437, 2,447 | 36.81, 37.00, 36.38 | 99.33%/99.36%/99.35% · 29.21%/30.08%/30.01% · 6.55%/6.61%/6.68% |
| get_large | 64 | Reference | 640 | 100.08 | 147.51 | 37,807 | 0 | 0 | 287.8 MiB | 601, 640, 640 | 171.21, 147.51, 140.70 | 100.42%/100.52%/101.53% · 8.77%/8.52%/8.83% · 2.50%/2.45%/2.31% |
| get_large | 256 | Rust | 2,322 | 109.62 | 140.52 | 140,081 | 0 | 0 | 130.8 MiB | 2,304, 2,322, 2,343 | 140.52, 139.60, 146.89 | 100.00%/99.29%/99.05% · 29.03%/29.83%/29.64% · 2.34%/3.00%/2.66% |
| get_large | 256 | Reference | 588 | 401.14 | 2,750.24 | 35,789 | 0 | 0 | 301.0 MiB | 544, 588, 622 | 2,383.20, 2,750.24, 2,894.15 | 100.35%/100.11%/100.42% · 6.53%/6.92%/8.43% · 1.69%/1.98%/1.93% |
| reject_401 | 64 | Rust | 18,832 | 3.23 | 5.01 | 1,124,729 | 0 | 0 | 19.6 MiB | 18,495, 18,832, 18,897 | 5.11, 5.01, 4.99 | 101.29%/101.29%/100.98% · 0.00%/0.00%/0.00% · 36.91%/35.85%/35.70% |
| reject_401 | 64 | Reference | 7,082 | 8.63 | 14.73 | 418,360 | 0 | 0 | 163.0 MiB | 6,714, 7,113, 7,082 | 15.66, 14.42, 14.73 | 100.58%/100.15%/100.37% · 0.00%/0.00%/0.00% · 17.77%/17.48%/17.39% |
| reject_401 | 256 | Rust | 21,889 | 11.34 | 15.13 | 1,315,036 | 0 | 0 | 28.6 MiB | 21,856, 21,889, 21,973 | 15.13, 15.28, 14.56 | 100.66%/100.57%/100.31% · 0.00%/0.00%/0.00% · 12.87%/13.05%/12.82% |
| reject_401 | 256 | Reference | 6,972 | 35.79 | 46.41 | 408,304 | 0 | 0 | 170.5 MiB | 7,086, 6,972, 6,325 | 46.41, 44.84, 81.74 | 100.45%/100.48%/100.48% · 0.00%/0.01%/0.00% · 16.91%/17.08%/16.63% |
| static | 64 | Rust | 25,070 | 2.39 | 4.11 | 1,491,868 | 0 | 0 | 23.1 MiB | 24,400, 25,106, 25,070 | 4.19, 3.95, 4.11 | 101.39%/101.49%/101.45% · 28.21%/30.64%/31.41% · 0.32%/0.31%/0.28% |
| static | 64 | Reference | 7,255 | 5.74 | 39.30 | 426,584 | 0 | 0 | 225.7 MiB | 7,303, 7,255, 6,731 | 39.08, 39.30, 42.68 | 100.25%/100.69%/100.48% · 7.62%/7.60%/7.77% · 0.28%/0.29%/0.29% |
| static | 256 | Rust | 19,931 | 12.75 | 19.85 | 1,157,436 | 0 | 0 | 34.5 MiB | 17,923, 19,931, 19,971 | 46.10, 19.85, 19.72 | 100.86%/101.30%/100.97% · 25.79%/28.35%/28.79% · 0.37%/0.36%/0.38% |
| static | 256 | Reference | 5,557 | 37.13 | 103.65 | 327,588 | 0 | 0 | 228.6 MiB | 5,093, 5,557, 5,702 | 157.07, 103.65, 96.66 | 100.90%/100.99%/100.60% · 8.27%/7.99%/9.17% · 0.33%/0.31%/0.32% |

#### 4 CPUs per gateway (CPU set 0-3)

| scenario | conc | gateway | req/s | p50 ms | p99 ms | checked | non-expected | errors | peak RSS | req/s of each run | p99 ms of each run | CPU % |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **get_small** | 64 | Rust | 29,456 | 2.07 | 4.33 | 1,775,677 | 0 | 0 | 28.5 MiB | 27,867, 29,456, 31,436 | 4.54, 4.33, 3.52 | 359.77%/380.57%/378.49% · 58.16%/48.29%/66.11% · 45.52%/35.47%/50.25% |
| **get_small** | 64 | Reference | 5,085 | 11.81 | 20.67 | 304,040 | 0 | 0 | 229.1 MiB | 5,001, 5,107, 5,085 | 21.66, 20.67, 19.51 | 126.53%/126.11%/126.28% · 7.19%/7.35%/7.49% · 14.53%/15.17%/15.04% |
| **get_small** | 256 | Rust | 31,810 | 7.97 | 11.70 | 1,896,465 | 0 | 0 | 40.1 MiB | 31,146, 31,821, 31,810 | 12.53, 11.70, 11.70 | 381.37%/382.40%/382.39% · 48.96%/47.64%/47.72% · 24.23%/23.58%/24.14% |
| **get_small** | 256 | Reference | 5,128 | 47.94 | 67.42 | 308,440 | 0 | 0 | 232.1 MiB | 5,099, 5,128, 5,173 | 66.14, 68.23, 67.42 | 131.32%/129.07%/128.00% · 5.22%/5.29%/5.21% · 13.86%/12.08%/12.08% |
| post_json | 64 | Rust | 26,732 | 2.37 | 3.47 | 1,602,810 | 0 | 0 | 29.6 MiB | 26,752, 26,732, 26,636 | 3.44, 3.47, 3.55 | 377.44%/373.36%/375.65% · 59.20%/58.49%/57.98% · 40.80%/40.88%/40.48% |
| post_json | 64 | Reference | 4,997 | 12.44 | 20.17 | 298,043 | 0 | 0 | 196.2 MiB | 4,894, 4,997, 5,002 | 21.04, 20.17, 19.52 | 126.89%/125.88%/125.75% · 8.48%/8.39%/8.43% · 14.64%/14.61%/14.86% |
| post_json | 256 | Rust | 24,673 | 10.29 | 14.24 | 1,482,215 | 0 | 0 | 44.9 MiB | 25,315, 24,673, 24,078 | 14.24, 14.19, 17.69 | 372.08%/373.61%/341.99% · 55.80%/56.19%/51.97% · 28.69%/28.42%/27.63% |
| post_json | 256 | Reference | 4,767 | 51.72 | 68.50 | 266,591 | 0 | 0 | 209.8 MiB | 4,779, 4,767, 3,753 | 68.45, 68.50, 86.95 | 133.54%/130.59%/129.83% · 8.36%/8.32%/9.95% · 13.88%/13.50%/13.26% |
| get_large | 64 | Rust | 6,707 | 9.32 | 19.59 | 388,889 | 0 | 0 | 80.9 MiB | 6,707, 5,593, 7,133 | 19.59, 29.00, 16.77 | 381.70%/304.34%/379.94% · 113.07%/87.61%/118.51% · 24.71%/14.87%/28.65% |
| get_large | 64 | Reference | 765 | 82.29 | 110.91 | 46,008 | 0 | 0 | 299.9 MiB | 749, 765, 778 | 110.91, 110.41, 128.62 | 162.15%/159.71%/142.96% · 13.76%/13.70%/11.40% · 3.78%/4.26%/3.40% |
| get_large | 256 | Rust | 5,412 | 44.81 | 109.71 | 305,749 | 0 | 0 | 164.5 MiB | 4,027, 5,792, 5,412 | 140.03, 109.71, 96.75 | 336.56%/381.80%/363.67% · 100.22%/114.69%/114.78% · 9.82%/17.31%/14.63% |
| get_large | 256 | Reference | 750 | 306.70 | 834.99 | 47,197 | 0 | 0 | 340.5 MiB | 750, 715, 858 | 1,603.91, 834.99, 345.04 | 157.24%/155.42%/157.23% · 11.22%/11.72%/12.07% · 2.68%/2.81%/2.64% |
| reject_401 | 64 | Rust | 34,402 | 1.64 | 4.06 | 2,107,719 | 0 | 0 | 25.7 MiB | 30,885, 40,079, 34,402 | 9.64, 2.81, 4.06 | 355.72%/343.03%/278.54% · 0.00%/0.00%/0.00% · 39.45%/38.55%/35.15% |
| reject_401 | 64 | Reference | 6,115 | 9.88 | 18.48 | 367,432 | 0 | 0 | 160.9 MiB | 6,115, 6,417, 5,828 | 18.48, 18.36, 20.50 | 101.02%/100.61%/101.55% · 0.01%/0.00%/0.06% · 18.97%/18.27%/20.09% |
| reject_401 | 256 | Rust | 36,730 | 6.00 | 18.87 | 1,948,304 | 0 | 0 | 38.4 MiB | 36,730, 40,293, 20,289 | 18.87, 17.76, 29.52 | 368.74%/368.79%/224.38% · 0.02%/0.00%/0.00% · 30.73%/30.42%/20.04% |
| reject_401 | 256 | Reference | 7,026 | 35.34 | 46.74 | 414,066 | 0 | 0 | 168.5 MiB | 7,026, 7,048, 6,601 | 46.74, 45.84, 48.71 | 103.07%/102.12%/101.87% · 0.00%/0.00%/0.01% · 18.04%/17.54%/17.47% |
| static | 64 | Rust | 51,937 | 1.22 | 2.07 | 3,095,260 | 0 | 0 | 29.2 MiB | 50,861, 51,937, 51,941 | 2.14, 2.07, 2.07 | 378.59%/376.92%/380.38% · 81.31%/80.24%/83.01% · 0.33%/0.34%/0.31% |
| static | 64 | Reference | 9,060 | 6.31 | 12.35 | 554,111 | 0 | 0 | 194.1 MiB | 9,060, 8,978, 9,655 | 12.35, 12.44, 11.65 | 143.47%/148.42%/145.36% · 10.78%/10.42%/10.39% · 0.33%/0.31%/0.32% |
| static | 256 | Rust | 51,678 | 4.88 | 7.49 | 3,106,885 | 0 | 0 | 39.8 MiB | 51,678, 51,496, 52,110 | 7.49, 7.59, 7.45 | 373.17%/376.53%/378.88% · 71.15%/72.36%/72.57% · 0.34%/0.33%/0.33% |
| static | 256 | Reference | 8,893 | 27.50 | 40.29 | 532,546 | 0 | 0 | 234.1 MiB | 8,776, 8,918, 8,893 | 41.34, 40.29, 40.02 | 144.74%/144.91%/146.81% · 10.22%/10.14%/10.29% · 0.29%/0.36%/0.33% |

#### Rust compared with the Reference Gateway (medians)

| CPUs | scenario | conc | req/s Rust ÷ Reference | p99 Reference ÷ Rust | peak RSS Reference ÷ Rust |
|---|---|---|---|---|---|
| 1 | **get_small** | 64 | 3.05× | 7.42× | 9.57× |
| 1 | **get_small** | 256 | 3.48× | 4.75× | 6.80× |
| 1 | post_json | 64 | 3.10× | 7.57× | 7.96× |
| 1 | post_json | 256 | 3.35× | 4.39× | 5.40× |
| 1 | get_large | 64 | 3.81× | 4.01× | 4.60× |
| 1 | get_large | 256 | 3.95× | 19.57× | 2.30× |
| 1 | reject_401 | 64 | 2.66× | 2.94× | 8.32× |
| 1 | reject_401 | 256 | 3.14× | 3.07× | 5.97× |
| 1 | static | 64 | 3.46× | 9.57× | 9.77× |
| 1 | static | 256 | 3.59× | 5.22× | 6.63× |
| 4 | **get_small** | 64 | 5.79× | 4.78× | 8.03× |
| 4 | **get_small** | 256 | 6.20× | 5.76× | 5.79× |
| 4 | post_json | 64 | 5.35× | 5.82× | 6.63× |
| 4 | post_json | 256 | 5.18× | 4.81× | 4.67× |
| 4 | get_large | 64 | 8.77× | 5.66× | 3.71× |
| 4 | get_large | 256 | 7.22× | 7.61× | 2.07× |
| 4 | reject_401 | 64 | 5.63× | 4.55× | 6.25× |
| 4 | reject_401 | 256 | 5.23× | 2.48× | 4.39× |
| 4 | static | 64 | 5.73× | 5.97× | 6.65× |
| 4 | static | 256 | 5.81× | 5.38× | 5.87× |

Scenarios: `get_small`: GET of a Routed Request answered with a 110-byte JSON body, expects 200, upstream receives every request, with `auth-user-id`, Buckets: Global IP, Custom; `post_json`: POST of a Routed Request with a 4 KB JSON body; the Route has one rate limit, expects 201, upstream receives every request, with `auth-user-id`, Buckets: Global IP, Custom, Route; `get_large`: GET of a Routed Request answered with a 1 MiB body (streaming), expects 200, upstream receives every request, with `auth-user-id`, Buckets: Global IP, Custom; `reject_401`: A Routed Request whose token has a bad signature, rejected by the middleware, expects 401, upstream receives nothing, Buckets: Global IP, Anonymous IP; `static`: A Static Request (no Route, middleware or rate limit); its forged auth-user-id must be dropped, expects 200, upstream receives every request, without `auth-user-id`, Buckets: none.
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
- **Middleware (EX-05):** JWT verification (HS256) in the gateway, `auth-user-id` from the token's subject, Identity = user id or client address. Confirmed by the user on 2026-11-18 as representative of their production middleware.

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
