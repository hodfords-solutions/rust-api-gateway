#!/usr/bin/env python3
"""End-to-end load test: rust-gateway-server against the Reference Gateway, in Docker.

    python3 benches/scripts/bench.py            # the full benchmark (about an hour)
    python3 benches/scripts/bench.py --smoke    # a few seconds per point (about 9 minutes)

See benches/README.md for what is measured and how to read the results. Only the standard
library is used. Everything runs in the Docker Compose project `rgw-bench`; nothing else on
the machine is started, stopped or touched.

For each CPU configuration, scenario (benches/scenarios.jsonl) and concurrency level, a
"point":

1. both gateways are (re)started with the same CPU set and memory limit, so peak RSS is per
   point, and Redis is flushed;
2. one request of the scenario is sent to each gateway: both must answer the expected
   status with the same body;
3. each gateway is warmed up, then the measured runs alternate between the gateways in
   ABBA order. The gateway not being measured is paused (`docker pause`), so it uses no CPU;
4. before each run the upstream's counters are reset; after it they must match what oha
   saw (every response had the expected status, and the upstream received exactly one
   request per response, with or without the middleware's `auth-user-id` as the scenario
   says);
5. peak RSS (`VmHWM` of PID 1) and the cgroup's peak memory are read from both gateways.

The mock upstream is also measured directly with the same load, to show it is not the
bottleneck. Raw results go to benches/results/<UTC time>-<mode>/; benches/scripts/summarize.py
turns them into the tables of benches/RESULTS.md (this script calls it at the end).
"""

from __future__ import annotations

import argparse
import base64
import datetime as dt
import hashlib
import hmac
import json
import os
import platform
import secrets
import shutil
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

BENCHES = Path(__file__).resolve().parent.parent
REPO = BENCHES.parent
COMPOSE = ["docker", "compose", "-f", str(BENCHES / "docker-compose.yml")]
PROJECT = "rgw-bench"
GATEWAYS = ["rust", "reference"]
PORTS = {"rust": 47080, "reference": 47081}
UPSTREAM_PORT = 47050
OHA_IMAGE = "ghcr.io/hatoo/oha:1.17.0"
# The Bearer token's subject; the upstream only checks that `auth-user-id` is present.
SUBJECT = "user-42"

FULL = dict(configs="1,4", concurrency="64,256", runs=3, duration=20, warmup=5)
SMOKE = dict(configs="1,4", concurrency="64,256", runs=2, duration=3, warmup=1)


def log(message: str) -> None:
    print(f"[{dt.datetime.now().strftime('%H:%M:%S')}] {message}", flush=True)


def run(cmd: list[str], *, env: dict | None = None, check: bool = True, capture: bool = True,
        input: bytes | None = None) -> subprocess.CompletedProcess:
    result = subprocess.run(cmd, env=env, capture_output=capture, input=input)
    if check and result.returncode != 0:
        err = result.stderr.decode(errors="replace") if capture else ""
        raise RuntimeError(f"command failed ({result.returncode}): {' '.join(cmd)}\n{err}")
    return result


def output(cmd: list[str], **kwargs) -> str:
    try:
        return run(cmd, **kwargs).stdout.decode(errors="replace").strip()
    except (RuntimeError, FileNotFoundError) as error:
        return f"unavailable ({str(error).splitlines()[0]})"


# --- JWT (HS256), generated here so the run needs no other tool ---------------------------

def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def jwt(claims: dict, secret: str) -> str:
    header = b64url(json.dumps({"alg": "HS256", "typ": "JWT"}, separators=(",", ":")).encode())
    payload = b64url(json.dumps(claims, separators=(",", ":")).encode())
    signature = hmac.new(secret.encode(), f"{header}.{payload}".encode(), hashlib.sha256).digest()
    return f"{header}.{payload}.{b64url(signature)}"


# --- Docker ---------------------------------------------------------------------------------

class Stack:
    """The `rgw-bench` Compose project, with the CPU layout of one configuration."""

    def __init__(self, args, secret: str):
        self.args = args
        self.env = dict(os.environ)
        self.env.update({
            "BENCH_JWT_SECRET": secret,
            "RGW_CONFIG": str(BENCHES / "fixtures" / "gateway-config.json"),
            "RGW_GW_MEMORY": args.gateway_memory,
            "RGW_UPSTREAM_CPUS": args.upstream_cpus,
            "RGW_UPSTREAM_THREADS": str(cpu_count(args.upstream_cpus)),
            "RGW_REDIS_CPUS": args.redis_cpus,
            "RGW_OHA_CPUS": args.oha_cpus,
            "RGW_GW_CPUS": "0",
        })

    def compose(self, *cmd: str, check: bool = True, capture: bool = True) -> subprocess.CompletedProcess:
        return run(COMPOSE + list(cmd), env=self.env, check=check, capture=capture)

    def container(self, service: str) -> str:
        return f"{PROJECT}-{service}-1"

    def set_gateway_cpus(self, cpus: str) -> None:
        self.env["RGW_GW_CPUS"] = cpus

    def oha(self, url: str, scenario: dict, concurrency: int, seconds: float, token: str,
            bad_token: str) -> dict:
        cmd = ["run", "--rm", "--no-deps", "-T", "oha",
               "-z", f"{seconds}s", "-c", str(concurrency), "--no-tui", "--output-format", "json",
               # Wait for the requests in flight at the deadline, so every request oha sent is
               # counted and the upstream's count can be compared exactly.
               "-w", "-m", scenario["method"]]
        for name, value in scenario.get("headers", {}).items():
            value = value.replace("${TOKEN}", token).replace("${BAD_TOKEN}", bad_token)
            cmd += ["-H", f"{name}: {value}"]
        if scenario.get("body_file"):
            cmd += ["-D", f"/fixtures/{scenario['body_file']}"]
        cmd.append(url)
        result = self.compose(*cmd)
        return json.loads(result.stdout)


def cpu_count(cpuset: str) -> int:
    count = 0
    for part in cpuset.split(","):
        if "-" in part:
            a, b = part.split("-")
            count += int(b) - int(a) + 1
        else:
            count += 1
    return count


def cpu_max(cpuset: str) -> int:
    return max(int(x) for part in cpuset.split(",") for x in part.split("-"))


def http(method: str, url: str, headers: dict | None = None, body: bytes | None = None,
         timeout: float = 10) -> tuple[int, bytes]:
    request = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


def upstream_stats() -> dict:
    return json.loads(http("GET", f"http://127.0.0.1:{UPSTREAM_PORT}/__stats")[1])


def reset_upstream() -> None:
    http("POST", f"http://127.0.0.1:{UPSTREAM_PORT}/__stats/reset")


def wait_for(url: str, what: str, container: str, timeout: float = 120) -> bytes:
    deadline = time.time() + timeout
    last = ""
    while time.time() < deadline:
        if output(["docker", "inspect", "-f", "{{.State.Running}}", container]) != "true":
            logs = output(["sh", "-c", f"docker logs --tail 20 {container} 2>&1"])
            raise RuntimeError(f"{what} is not running:\n{logs}")
        try:
            status, body = http("GET", url, timeout=5)
            if status == 200:
                return body
            last = f"status {status}"
        except OSError as error:
            last = str(error)
        time.sleep(0.25)
    raise RuntimeError(f"{what} did not become ready at {url}: {last}")


def docker_stats(names: list[str]) -> dict:
    """CPU and memory of each container, from one `docker stats` sample."""
    out = output(["docker", "stats", "--no-stream", "--format", "{{json .}}"] + names)
    samples = {}
    for line in out.splitlines():
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        samples[row.get("Name", "?")] = {"cpu": row.get("CPUPerc"), "mem": row.get("MemUsage")}
    return samples


class Sampler(threading.Thread):
    """Takes one `docker stats` sample halfway through a run."""

    def __init__(self, delay: float, names: list[str]):
        super().__init__(daemon=True)
        self.delay, self.names, self.result = delay, names, {}

    def run(self) -> None:
        time.sleep(self.delay)
        self.result = docker_stats(self.names)


# --- Environment ---------------------------------------------------------------------------

def host_description() -> dict:
    info: dict = {"platform": platform.platform(), "machine": platform.machine()}
    if sys.platform == "darwin":
        info["cpu"] = output(["sysctl", "-n", "machdep.cpu.brand_string"])
        info["cpus"] = output(["sysctl", "-n", "hw.ncpu"])
        info["memory_bytes"] = output(["sysctl", "-n", "hw.memsize"])
        info["os"] = f"macOS {output(['sw_vers', '-productVersion'])}"
    else:
        try:
            for line in Path("/proc/cpuinfo").read_text().splitlines():
                if line.lower().startswith("model name"):
                    info["cpu"] = line.split(":", 1)[1].strip()
                    break
        except OSError:
            pass
        info["cpus"] = str(os.cpu_count())
        try:
            release = dict(line.split("=", 1) for line in Path("/etc/os-release").read_text().splitlines() if "=" in line)
            info["os"] = release.get("PRETTY_NAME", "").strip('"')
        except OSError:
            pass
    info["kernel"] = output(["uname", "-sr"])
    return info


def load_average(stack: Stack | None) -> dict:
    load = {"host": [round(x, 2) for x in os.getloadavg()]}
    if stack is not None:
        vm = output(["docker", "exec", stack.container("redis"), "cat", "/proc/loadavg"])
        load["docker_vm"] = vm.split()[:3]
    return load


def environment(stack: Stack, args) -> dict:
    commit = output(["git", "-C", str(REPO), "rev-parse", "HEAD"])
    dirty = output(["git", "-C", str(REPO), "status", "--porcelain"])
    version_line = next((l for l in (REPO / "Cargo.toml").read_text().splitlines() if l.startswith("version")), "")
    return {
        "started": dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
        "mode": "smoke" if args.smoke else "full",
        "host": host_description(),
        "docker": {
            "version": output(["docker", "version", "--format", "{{.Server.Version}} ({{.Server.Os}}/{{.Server.Arch}})"]),
            "vm_cpus": output(["docker", "info", "--format", "{{.NCPU}}"]),
            "vm_memory_bytes": output(["docker", "info", "--format", "{{.MemTotal}}"]),
            "vm_os": output(["docker", "info", "--format", "{{.OperatingSystem}}, kernel {{.KernelVersion}}"]),
        },
        "rust": output(["docker", "run", "--rm", "--entrypoint", "cat", "rgw-bench-rust", "/usr/local/share/rust-gateway/rustc-version"]),
        "node": output(["docker", "run", "--rm", "rgw-bench-reference", "node", "--version"]),
        "gateways": {
            "rust": f"rust-gateway-server {version_line.split('=')[-1].strip().strip(chr(34))} at {commit[:12]}",
            "reference": "@hodfords/nestjs-api-gateway " + output([
                "docker", "run", "--rm", "rgw-bench-reference", "node", "-e",
                "console.log(JSON.parse(require('fs').readFileSync('node_modules/@hodfords/nestjs-api-gateway/package.json','utf8')).version)"]),
        },
        "redis": output(["docker", "exec", stack.container("redis"), "redis-server", "--version"]),
        "oha": output(["docker", "run", "--rm", OHA_IMAGE, "--version"]),
        "commit": commit,
        "worktree_dirty": bool(dirty) and not dirty.startswith("unavailable"),
        "layout": {
            "gateway_cpus": {n: gateway_cpus(int(n)) for n in args.configs.split(",")},
            "gateway_memory": args.gateway_memory,
            "upstream_cpus": args.upstream_cpus,
            "redis_cpus": args.redis_cpus,
            "oha_cpus": args.oha_cpus,
            "reference_processes": 1,
        },
        "settings": {
            "concurrency": [int(c) for c in args.concurrency.split(",")],
            "runs": args.runs, "duration_s": args.duration, "warmup_s": args.warmup,
            "scenarios": [s["name"] for s in scenarios(args)],
        },
    }


def gateway_cpus(n: int) -> str:
    return "0" if n == 1 else f"0-{n - 1}"


def scenarios(args) -> list[dict]:
    rows = [json.loads(line) for line in (BENCHES / "scenarios.jsonl").read_text().splitlines() if line.strip()]
    if args.scenarios:
        wanted = args.scenarios.split(",")
        rows = [r for r in rows if r["name"] in wanted]
    return rows


def upstream_path(path: str, static: bool) -> str:
    """The path the gateways forward: Service Prefix removed, trailing slash unless static."""
    rest = path.removeprefix("/svc")
    return rest if static or rest.endswith("/") else rest + "/"


def check_upstream(scenario: dict, responses: int, stats: dict) -> str | None:
    kind = scenario["upstream"]
    expected = {
        "with_user": {"withUser": responses, "withoutUser": 0},
        "without_user": {"withUser": 0, "withoutUser": responses},
        "none": {"withUser": 0, "withoutUser": 0},
    }[kind]
    got = {k: stats.get(k) for k in expected}
    return None if got == expected else f"upstream saw {got}, expected {expected}"


def check_oha(scenario: dict, result: dict) -> tuple[int, str | None]:
    statuses = {int(k): v for k, v in (result.get("statusCodeDistribution") or {}).items()}
    errors = result.get("errorDistribution") or {}
    responses = sum(statuses.values())
    problems = []
    if responses == 0:
        problems.append("no responses")
    unexpected = {k: v for k, v in statuses.items() if k != scenario["expect_status"]}
    if unexpected:
        problems.append(f"unexpected statuses {unexpected}")
    if errors:
        problems.append(f"errors {errors}")
    return responses, "; ".join(problems) or None


# --- The benchmark -------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--smoke", action="store_true", help="a few seconds per point; results go to the smoke section")
    parser.add_argument("--configs", help="gateway CPU counts, comma-separated (default 1,4)")
    parser.add_argument("--concurrency", help="connections, comma-separated (default 64,256)")
    parser.add_argument("--runs", type=int, help="measured runs per gateway per point (default 3; smoke 2)")
    parser.add_argument("--duration", type=float, help="seconds per run (default 20; smoke 3)")
    parser.add_argument("--warmup", type=float, help="warm-up seconds per gateway per point (default 5; smoke 1)")
    parser.add_argument("--scenarios", help="only these scenarios, comma-separated")
    parser.add_argument("--upstream-cpus", default="4-5")
    parser.add_argument("--redis-cpus", default="6")
    parser.add_argument("--oha-cpus", default="7-9")
    parser.add_argument("--gateway-memory", default="1g")
    parser.add_argument("--no-build", action="store_true", help="use the images already built")
    parser.add_argument("--no-results-md", action="store_true", help="do not update benches/RESULTS.md")
    parser.add_argument("--busy-load", type=float, default=1.0,
                        help="host 1-minute load average above which the run is marked as taken on a busy machine")
    args = parser.parse_args()
    for key, value in (SMOKE if args.smoke else FULL).items():
        if getattr(args, key) is None:
            setattr(args, key, value)
    if args.runs < 1:
        parser.error("--runs must be at least 1")

    if shutil.which("docker") is None:
        print("docker is required", file=sys.stderr)
        return 2
    vm_cpus = int(output(["docker", "info", "--format", "{{.NCPU}}"]) or 0)
    configs = [int(n) for n in args.configs.split(",")]
    needed = max([cpu_max(gateway_cpus(max(configs))), cpu_max(args.upstream_cpus),
                  cpu_max(args.redis_cpus), cpu_max(args.oha_cpus)]) + 1
    if vm_cpus < needed:
        print(f"Docker has {vm_cpus} CPUs; this layout needs {needed}. Adjust --configs and the CPU sets.", file=sys.stderr)
        return 2
    gw_top = cpu_max(gateway_cpus(max(configs)))
    for name, cpuset in [("upstream", args.upstream_cpus), ("redis", args.redis_cpus), ("oha", args.oha_cpus)]:
        if min(int(x) for part in cpuset.split(",") for x in part.split("-")) <= gw_top:
            print(f"the {name} CPU set {cpuset} overlaps the gateway's 0-{gw_top}", file=sys.stderr)
            return 2

    secret = secrets.token_hex(32)
    now = int(time.time())
    claims = {"sub": SUBJECT, "iat": now, "exp": now + 7 * 24 * 3600}
    token = jwt(claims, secret)
    bad_token = jwt(claims, secrets.token_hex(32))
    stack = Stack(args, secret)
    rows = scenarios(args)
    concurrencies = [int(c) for c in args.concurrency.split(",")]

    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    out = BENCHES / "results" / f"{stamp}-{'smoke' if args.smoke else 'full'}"
    raw = out / "raw"
    raw.mkdir(parents=True)
    log(f"results: {out.relative_to(REPO)}")

    load_before = load_average(None)
    if not args.no_build:
        log("building images")
        stack.compose("build", "upstream", "rust", "reference", capture=False)
    run(["docker", "pull", "-q", OHA_IMAGE])
    try:
        stack.compose("down", "--remove-orphans", check=False)
        stack.compose("up", "-d", "redis", "upstream")
        wait_for(f"http://127.0.0.1:{UPSTREAM_PORT}/__stats", "the upstream", stack.container("upstream"))
        env = environment(stack, args)
        env["load_before"] = load_before
        env["load_before"].update({k: v for k, v in load_average(stack).items() if k != "host"})
        env["busy"] = load_before["host"][0] > args.busy_load
        if env["busy"]:
            log(f"WARNING: host load average is {load_before['host']}; results will be marked as taken on a busy machine")
        (out / "env.json").write_text(json.dumps(env, indent=2) + "\n")
        (out / "scenarios.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rows))
        failures = measure(stack, args, rows, configs, concurrencies, token, bad_token, raw)
    finally:
        log("stopping the rgw-bench containers")
        try:
            env_after = json.loads((out / "env.json").read_text()) if (out / "env.json").exists() else {}
            env_after["load_after"] = load_average(stack)
            env_after["finished"] = dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds")
            (out / "env.json").write_text(json.dumps(env_after, indent=2) + "\n")
        finally:
            stack.compose("unpause", "rust", "reference", check=False)
            stack.compose("down", "--remove-orphans", check=False)

    summarize = [sys.executable, str(BENCHES / "scripts" / "summarize.py"), str(out)]
    if not args.no_results_md:
        summarize += ["--results-md", str(BENCHES / "RESULTS.md")]
    result = run(summarize, capture=False, check=False)
    if failures:
        log(f"{len(failures)} point(s) failed their checks and are not reported:")
        for failure in failures:
            log(f"  {failure}")
        return 1
    return result.returncode


def measure(stack: Stack, args, rows, configs, concurrencies, token, bad_token, raw: Path) -> list[str]:
    failures: list[str] = []
    names = [stack.container(s) for s in ("rust", "reference", "upstream", "redis")]

    # The upstream alone, with the same load generator: its ceiling for each forwarded shape.
    for scenario in rows:
        if scenario["upstream"] == "none":
            continue
        static = scenario["upstream"] == "without_user"
        url = f"http://upstream:5000{upstream_path(scenario['path'], static)}"
        for c in concurrencies:
            log(f"upstream-direct {scenario['name']} c{c}")
            stack.oha(url, scenario, c, args.warmup, token, bad_token)
            for r in range(1, args.runs + 1):
                sampler = Sampler(args.duration / 2, names[2:])
                sampler.start()
                result = stack.oha(url, scenario, c, args.duration, token, bad_token)
                sampler.join()
                result["_samples"] = sampler.result
                (raw / f"upstream-direct-{scenario['name']}-c{c}-r{r}.json").write_text(json.dumps(result))

    for n in configs:
        stack.set_gateway_cpus(gateway_cpus(n))
        for scenario in rows:
            for c in concurrencies:
                point = f"cpu{n}-{scenario['name']}-c{c}"
                log(f"point {point}")
                failure = measure_point(stack, args, scenario, n, c, token, bad_token, raw, point, names)
                if failure:
                    log(f"  FAILED: {failure}")
                    failures.append(f"{point}: {failure}")
    return failures


def measure_point(stack, args, scenario, n, c, token, bad_token, raw: Path, point: str, names) -> str | None:
    stack.compose("rm", "-sf", "rust", "reference", check=False)
    stack.compose("up", "-d", "--force-recreate", "--no-deps", "rust", "reference")
    for gw in GATEWAYS:
        body = wait_for(f"http://127.0.0.1:{PORTS[gw]}/__bench/ready", f"the {gw} gateway", stack.container(gw))
        if json.loads(body).get("services") != ["svc"]:
            return f"{gw} did not load the benchmark document: {body!r}"
    run(["docker", "exec", stack.container("redis"), "redis-cli", "FLUSHALL"])

    # One request to each gateway: the expected status, and the same body from both.
    headers = {k: v.replace("${TOKEN}", token).replace("${BAD_TOKEN}", bad_token)
               for k, v in scenario.get("headers", {}).items()}
    body = (BENCHES / "fixtures" / scenario["body_file"]).read_bytes() if scenario.get("body_file") else None
    seen = {}
    for gw in GATEWAYS:
        status, response = http(scenario["method"], f"http://127.0.0.1:{PORTS[gw]}{scenario['path']}", headers, body)
        seen[gw] = {"status": status, "bytes": len(response), "sha256": hashlib.sha256(response).hexdigest()}
        if scenario["expect_status"] == 401:
            seen[gw]["body"] = response.decode(errors="replace")
    (raw / f"{point}.precheck.json").write_text(json.dumps(seen, indent=2))
    for gw in GATEWAYS:
        if seen[gw]["status"] != scenario["expect_status"]:
            return f"pre-check: {gw} answered {seen[gw]['status']}, expected {scenario['expect_status']}"
    if seen["rust"]["sha256"] != seen["reference"]["sha256"]:
        return f"pre-check: the gateways answered different bodies: {seen}"

    url_for = {gw: f"http://{gw}:3000{scenario['path']}" for gw in GATEWAYS}
    stack.compose("pause", "rust", "reference")
    for gw in GATEWAYS:
        stack.compose("unpause", gw)
        stack.oha(url_for[gw], scenario, c, args.warmup, token, bad_token)
        stack.compose("pause", gw)

    problem = None
    for r in range(1, args.runs + 1):
        order = GATEWAYS if r % 2 == 1 else list(reversed(GATEWAYS))
        for gw in order:
            stack.compose("unpause", gw)
            reset_upstream()
            sampler = Sampler(args.duration / 2, names)
            sampler.start()
            result = stack.oha(url_for[gw], scenario, c, args.duration, token, bad_token)
            sampler.join()
            stats = upstream_stats()
            stack.compose("pause", gw)
            responses, oha_problem = check_oha(scenario, result)
            upstream_problem = check_upstream(scenario, responses, stats)
            result["_upstream"] = stats
            result["_samples"] = sampler.result
            result["_check"] = {"oha": oha_problem, "upstream": upstream_problem}
            (raw / f"{point}-{gw}-r{r}.json").write_text(json.dumps(result))
            for p in (oha_problem, upstream_problem):
                if p and problem is None:
                    problem = f"{gw} run {r}: {p}"

    stack.compose("unpause", "rust", "reference")
    for gw in GATEWAYS:
        status = output(["docker", "exec", stack.container(gw), "cat", "/proc/1/status"])
        hwm = next((line.split(":", 1)[1].strip() for line in status.splitlines() if line.startswith("VmHWM")), None)
        peak = output(["docker", "exec", stack.container(gw), "cat", "/sys/fs/cgroup/memory.peak"])
        (raw / f"{point}-{gw}.memory.json").write_text(json.dumps({"vmhwm": hwm, "cgroup_peak_bytes": peak}))
    stack.compose("rm", "-sf", "rust", "reference", check=False)
    return problem


if __name__ == "__main__":
    sys.exit(main())
