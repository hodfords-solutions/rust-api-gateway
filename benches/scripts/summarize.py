#!/usr/bin/env python3
"""Turns one end-to-end benchmark run (benches/results/<run>/) into Markdown tables.

    python3 benches/scripts/summarize.py benches/results/<run>
    python3 benches/scripts/summarize.py benches/results/<run> --results-md benches/RESULTS.md

With --results-md, the tables replace the smoke or official section of that file (chosen by
the run's mode) between its `<!-- BEGIN smoke -->`/`<!-- END smoke -->` or
`<!-- BEGIN official -->`/`<!-- END official -->` markers; otherwise they are printed.
The tables are also written to <run>/summary.md.

A point is reported only when every run of both gateways passed its checks (only the
expected status, no errors, and the upstream's count equal to the responses). Otherwise the
row says why it was rejected and shows no numbers. The exit status is 1 when any point was
rejected.
"""

from __future__ import annotations

import argparse
import json
import re
import statistics
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench import check_oha, check_upstream  # noqa: E402  (the same checks as the run)

GATEWAYS = ["rust", "reference"]
LABEL = {"rust": "Rust", "reference": "Reference"}


def load(path: Path):
    return json.loads(path.read_text())


def median(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def ms(seconds):
    return None if seconds is None else seconds * 1000


def fmt(value, digits=0, suffix=""):
    if value is None:
        return "–"
    return f"{value:,.{digits}f}{suffix}"


def kib_to_mib(text):
    match = re.match(r"(\d+)\s*kB", text or "")
    return int(match.group(1)) / 1024 if match else None


def run_numbers(result: dict) -> dict:
    summary = result.get("summary") or {}
    latency = result.get("latencyPercentiles") or {}
    statuses = {int(k): v for k, v in (result.get("statusCodeDistribution") or {}).items()}
    return {
        "rps": summary.get("requestsPerSec"),
        "p50": ms(latency.get("p50")),
        "p99": ms(latency.get("p99")),
        "responses": sum(statuses.values()),
        "statuses": statuses,
        "errors": sum((result.get("errorDistribution") or {}).values()),
        "upstream": result.get("_upstream"),
        "samples": result.get("_samples") or {},
    }


def cpu_samples(runs: list[dict], container_suffixes: list[str]) -> str:
    parts = []
    for suffix in container_suffixes:
        values = []
        for run in runs:
            for name, sample in run["samples"].items():
                if name.endswith(f"-{suffix}-1") and sample.get("cpu"):
                    values.append(sample["cpu"])
        parts.append("/".join(values) if values else "–")
    return " · ".join(parts)


def point_rows(raw: Path, point: str, scenario: dict, runs: int):
    """One row per gateway, or a rejection reason for the whole point."""
    rows = {}
    problems = []
    precheck = raw / f"{point}.precheck.json"
    if not precheck.exists():
        return None, ["not measured"]
    for gw in GATEWAYS:
        files = sorted(raw.glob(f"{point}-{gw}-r*.json"), key=lambda p: int(p.stem.rsplit("-r", 1)[1]))
        results = [load(f) for f in files]
        numbers = [run_numbers(r) for r in results]
        if len(numbers) < runs:
            problems.append(f"{LABEL[gw]}: {len(numbers)} of {runs} runs")
        # Checked again from the raw data, not taken from what the run recorded.
        for i, result in enumerate(results, 1):
            responses, oha_problem = check_oha(scenario, result)
            if result.get("_upstream") is None:
                upstream_problem = "no upstream counters recorded"
            else:
                upstream_problem = check_upstream(scenario, responses, result["_upstream"])
            for problem in (oha_problem, upstream_problem):
                if problem:
                    problems.append(f"{LABEL[gw]} run {i}: {problem}")
        memory_file = raw / f"{point}-{gw}.memory.json"
        memory = load(memory_file) if memory_file.exists() else {}
        rows[gw] = {"runs": numbers, "memory": memory}
    if problems:
        return None, problems
    return rows, []


def describe_upstream(scenario: dict) -> str:
    return {
        "with_user": "every request, with `auth-user-id`",
        "without_user": "every request, without `auth-user-id`",
        "none": "nothing",
    }[scenario["upstream"]]


def summarize(run_dir: Path) -> tuple[str, int]:
    env = load(run_dir / "env.json")
    scenarios = [json.loads(l) for l in (run_dir / "scenarios.jsonl").read_text().splitlines() if l.strip()]
    scenarios.sort(key=lambda s: not s.get("headline"))
    settings = env["settings"]
    raw = run_dir / "raw"
    rejected = 0
    out = []
    w = out.append

    host = env["host"]
    docker = env["docker"]
    vm_mem = docker.get("vm_memory_bytes", "")
    vm_mem = f"{int(vm_mem) / 2**30:.1f} GiB" if vm_mem.isdigit() else vm_mem
    host_mem = host.get("memory_bytes", "")
    host_mem = f", {int(host_mem) / 2**30:.0f} GiB" if str(host_mem).isdigit() else ""
    w(f"Run `{run_dir.name}`, started {env['started']}, finished {env.get('finished', '?')}. "
      f"Raw results: `benches/results/{run_dir.name}/`.")
    w("")
    w(f"- **Machine:** {host.get('cpu', '?')} ({host.get('cpus', '?')} CPUs{host_mem}), {host.get('os', '?')} ({host.get('kernel', '')})")
    w(f"- **Docker:** {docker['version']}; VM with {docker['vm_cpus']} CPUs and {vm_mem} ({docker['vm_os']})")
    w(f"- **Gateways:** {env['gateways']['rust']} (built with {env['rust']}); "
      f"{env['gateways']['reference']} on Node {env['node']}, one process")
    w(f"- **Tools:** {env['oha']}; {env['redis']}")
    w(f"- **Commit:** `{env['commit']}`{' (with uncommitted changes)' if env.get('worktree_dirty') else ''}")
    before = env.get("load_before", {})
    after = env.get("load_after", {})
    w(f"- **Load average (1, 5, 15 min):** host {before.get('host')} before, {after.get('host')} after; "
      f"Docker VM {before.get('docker_vm')} before, {after.get('docker_vm')} after"
      + ("  **(busy machine: numbers are not comparable to a quiet run)**" if env.get("busy") else ""))
    layout = env["layout"]
    w(f"- **CPU sets:** gateway {', '.join(f'{v} ({k} CPU)' for k, v in layout['gateway_cpus'].items())}, "
      f"memory {layout['gateway_memory']}, the same for both gateways; upstream {layout['upstream_cpus']}; "
      f"Redis {layout['redis_cpus']}; oha {layout['oha_cpus']}. The gateway not being measured is paused.")
    w(f"- **Load:** {settings['runs']} runs of {settings['duration_s']} s per gateway per point, after "
      f"{settings['warmup_s']} s of warm-up, alternating between the gateways (ABBA); keep-alive "
      f"connections: {', '.join(map(str, settings['concurrency']))}.")
    w("- **Middleware (EX-05):** JWT verification (HS256) in the gateway, `auth-user-id` from the token's "
      "subject, Identity = user id or client address. *This is an assumption about production middleware "
      "that has not been confirmed.*")
    w("")
    w("Columns: req/s, p50 and p99 are medians over the runs (each run's req/s and p99 follow); "
      "*checked* = responses that all had the expected status and matched the upstream's count; "
      "peak RSS = `VmHWM` of the gateway process over the whole point (warm-up and all runs); "
      "CPU = `docker stats` halfway through each run, gateway · upstream · Redis.")
    w("")

    # The upstream measured directly.
    w("#### Mock upstream measured directly (its ceiling)")
    w("")
    w("| scenario | conc | req/s (median) | p99 ms | req/s of each run |")
    w("|---|---|---|---|---|")
    for scenario in scenarios:
        for c in settings["concurrency"]:
            files = sorted(raw.glob(f"upstream-direct-{scenario['name']}-c{c}-r*.json"))
            if not files:
                continue
            numbers = [run_numbers(load(f)) for f in files]
            w(f"| {scenario['name']} | {c} | {fmt(median(n['rps'] for n in numbers))} | "
              f"{fmt(median(n['p99'] for n in numbers), 2)} | {', '.join(fmt(n['rps']) for n in numbers)} |")
    w("")

    comparisons = []
    for n_cpus, cpus in layout["gateway_cpus"].items():
        w(f"#### {n_cpus} CPU{'s' if n_cpus != '1' else ''} per gateway (CPU set {cpus})")
        w("")
        w("| scenario | conc | gateway | req/s | p50 ms | p99 ms | checked | non-expected | errors | peak RSS | req/s of each run | p99 ms of each run | CPU % |")
        w("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
        for scenario in scenarios:
            for c in settings["concurrency"]:
                point = f"cpu{n_cpus}-{scenario['name']}-c{c}"
                rows, problems = point_rows(raw, point, scenario, settings["runs"])
                name = f"**{scenario['name']}**" if scenario.get("headline") else scenario["name"]
                if rows is None:
                    rejected += 1
                    w(f"| {name} | {c} | both | REJECTED: {'; '.join(problems)[:300]} |||||||||||")
                    continue
                medians = {}
                for gw in GATEWAYS:
                    runs = rows[gw]["runs"]
                    rps = median(r["rps"] for r in runs)
                    p50 = median(r["p50"] for r in runs)
                    p99 = median(r["p99"] for r in runs)
                    rss = kib_to_mib(rows[gw]["memory"].get("vmhwm"))
                    medians[gw] = (rps, p99, rss)
                    total = sum(r["responses"] for r in runs)
                    nonexp = sum(sum(v for k, v in r["statuses"].items() if k != scenario["expect_status"]) for r in runs)
                    errors = sum(r["errors"] for r in runs)
                    w(f"| {name} | {c} | {LABEL[gw]} | {fmt(rps)} | {fmt(p50, 2)} | {fmt(p99, 2)} | {total:,} | {nonexp} | {errors} | "
                      f"{fmt(rss, 1, ' MiB')} | {', '.join(fmt(r['rps']) for r in runs)} | {', '.join(fmt(r['p99'], 2) for r in runs)} | "
                      f"{cpu_samples(runs, [gw, 'upstream', 'redis'])} |")
                comparisons.append((n_cpus, scenario, c, medians))
        w("")

    w("#### Rust compared with the Reference Gateway (medians)")
    w("")
    w("| CPUs | scenario | conc | req/s Rust ÷ Reference | p99 Reference ÷ Rust | peak RSS Reference ÷ Rust |")
    w("|---|---|---|---|---|---|")
    for n_cpus, scenario, c, m in comparisons:
        def ratio(a, b):
            return f"{a / b:.2f}×" if a and b else "–"
        name = f"**{scenario['name']}**" if scenario.get("headline") else scenario["name"]
        w(f"| {n_cpus} | {name} | {c} | {ratio(m['rust'][0], m['reference'][0])} | "
          f"{ratio(m['reference'][1], m['rust'][1])} | {ratio(m['reference'][2], m['rust'][2])} |")
    w("")
    w("Scenarios: " + "; ".join(
        f"`{s['name']}`: {s['description']}, expects {s['expect_status']}, upstream receives {describe_upstream(s)}, "
        f"Buckets: {s['buckets']}" for s in scenarios) + ".")
    if rejected:
        w("")
        w(f"**{rejected} point(s) failed their checks and are not reported.**")
    return "\n".join(out) + "\n", rejected


def replace_section(results_md: Path, section: str, body: str) -> None:
    text = results_md.read_text()
    begin, end = f"<!-- BEGIN {section} -->", f"<!-- END {section} -->"
    if begin not in text or end not in text:
        raise SystemExit(f"{results_md} has no {begin} … {end} markers")
    head, rest = text.split(begin, 1)
    _, tail = rest.split(end, 1)
    results_md.write_text(f"{head}{begin}\n{body}{end}{tail}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("run", type=Path)
    parser.add_argument("--results-md", type=Path)
    args = parser.parse_args()
    body, rejected = summarize(args.run)
    (args.run / "summary.md").write_text(body)
    if args.results_md:
        mode = load(args.run / "env.json")["mode"]
        replace_section(args.results_md, "smoke" if mode == "smoke" else "official", body)
        print(f"updated the {'smoke' if mode == 'smoke' else 'official'} section of {args.results_md}")
    else:
        print(body)
    return 1 if rejected else 0


if __name__ == "__main__":
    sys.exit(main())
