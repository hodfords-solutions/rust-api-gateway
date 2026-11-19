#!/usr/bin/env python3
"""Writes seed inputs for every fuzz target under fuzz/corpus/<target>/seed-NNNN.

The seeds come from the golden data the unit tests already use (crates/rust-gateway/src/
**/testdata/*.json, conformance/fixtures/*.json) plus a few hand-written HTTP messages, in
the input format each target documents at its top. The output is deterministic; the
corpus directories are not committed (see fuzz/.gitignore), so run this before fuzzing:

    python3 fuzz/scripts/seed-corpora.py

Standard library only. Existing seed-* files are replaced; other corpus files are kept.
"""

import json
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[2]
CRATE = ROOT / "crates" / "rust-gateway" / "src"
CORPUS = ROOT / "fuzz" / "corpus"

# At most this many seeds per target, spread evenly over the golden cases.
LIMIT = 400


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def spread(items, limit=LIMIT):
    items = list(items)
    if len(items) <= limit:
        return items
    step = len(items) / limit
    return [items[int(i * step)] for i in range(limit)]


def write(target, seeds):
    directory = CORPUS / target
    directory.mkdir(parents=True, exist_ok=True)
    for old in directory.glob("seed-*"):
        old.unlink()
    unique = []
    seen = set()
    for seed in seeds:
        data = seed if isinstance(seed, bytes) else seed.encode("utf-8", "surrogatepass")
        if data not in seen:
            seen.add(data)
            unique.append(data)
    for i, data in enumerate(spread(unique)):
        (directory / f"seed-{i:04d}").write_bytes(data)
    print(f"{target}: {min(len(unique), LIMIT)} seeds")


def flatten(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for item in value:
            yield from flatten(item)


def documents():
    docs = [load(p) for p in sorted((ROOT / "conformance" / "fixtures").glob("*.json"))]
    for case in load(CRATE / "mcp" / "testdata" / "registry.json")["cases"]:
        docs.extend(case.get("services", {}).values())
    return docs


def main():
    # route_pattern: `patterns \0 path \0 path ...`
    regexp = load(CRATE / "route" / "testdata" / "path-to-regexp.json")
    seeds = []
    for entry in regexp["patterns"]:
        pattern = entry["pattern"]
        if isinstance(pattern, list):
            pattern = "\x1f".join(p for p in flatten(pattern))
        elif not isinstance(pattern, str):
            continue
        inputs = [c["input"] for c in entry.get("cases", []) if isinstance(c.get("input"), str)][:3]
        seeds.append("\0".join([pattern, *inputs]))
    write("route_pattern", seeds)

    # request_url: `target \0 service prefix`
    url_cases = load(CRATE / "request" / "testdata" / "url.json")["cases"]
    param_cases = load(CRATE / "request" / "testdata" / "path_param.json")["cases"]
    targets = [c[0] for c in url_cases] + [c[0] for c in param_cases]
    write("request_url", [f"{t}\0user-services" for t in targets] + [f"{t}\0svc" for t in targets[:50]])

    # request_query: the raw query
    write("request_query", [c[0] for c in load(CRATE / "request" / "testdata" / "query.json")["cases"]])

    # request_headers: `method \n peer \n name:value ...`
    seeds = []
    for case in load(CRATE / "request" / "testdata" / "headers.json")["cases"]:
        lines = ["GET", "::ffff:127.0.0.1"] + [f"{n}:{v}" for n, v in case[0]]
        seeds.append("\n".join(lines))
    seeds.append("DELETE\n::1\nconnection:keep-alive, x-foo\nx-foo:1\nx-forwarded-for:1.2.3.4\n@auth-user-id:42\n-cookie")
    write("request_headers", seeds)

    # openapi_document, documents_json, mcp_tools: a document (and, for openapi_document,
    # request URLs after a NUL byte).
    docs = [json.dumps(d) for d in documents()]
    urls = "\n".join(["/v1/users", "/v1/users/42", "/v1/users/%zz", "/svc/v1/users?x=1", "http://h/v1/users/a%2Fb"])
    write("openapi_document", [f"{d}\0{urls}" for d in docs] + ['{"paths":{"/a/{id}":{"get":{}}}}\0/a/1'])
    write("documents_json", docs + ['{"a":1,"0":2,"a":3,"s":"\\ud800","n":1e21}'])
    write("mcp_tools", docs)

    # mcp_call: the golden call cases as they are.
    write("mcp_call", [json.dumps(c) for c in load(CRATE / "mcp" / "testdata" / "call.json")["cases"]])

    # client_address: `trust \0 peer \0 x-forwarded-for \0 address \0 subnet`
    golden = load(CRATE / "rate_limit" / "testdata" / "golden.json")
    trusts = []
    for entry in golden["trustCompiled"]:
        trust = entry["trust"]
        if isinstance(trust, bool):
            trusts.append("true" if trust else "false")
        elif isinstance(trust, list):
            trusts.append(",".join(trust))
        else:
            trusts.append(str(trust))
    peers = [p or "" for p in golden["peers"]]
    forwarded = golden["forwardedFor"]
    normalize = golden["normalize"]
    seeds = []
    for i, trust in enumerate(trusts):
        peer = peers[i % len(peers)]
        xff = forwarded[i % len(forwarded)]
        n = normalize[(i * 13) % len(normalize)]
        fields = [trust, peer] + ([] if xff is None else [xff, n["input"], str(n["subnet"] or 64)])
        seeds.append("\0".join(fields))
    write("client_address", seeds)

    # inbound_stream: control byte, then client bytes.
    requests = [
        b"GET /a?b=c HTTP/1.1\r\nHost: h\r\n\r\n",
        b"GET /\xc3\xa9\"<>`{} HTTP/1.1\r\nHost: h\r\n\r\nGET /x HTTP/1.1\r\nHost: h\r\n\r\n",
        b"POST /p HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\n\r\nhelloGET /n HTTP/1.1\r\nHost: h\r\n\r\n",
        b"POST /c HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\nX-T: 1\r\n\r\nGET /after HTTP/1.1\r\n\r\n",
        b"GET /ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n\x81\x05hello",
        b"CONNECT h:443 HTTP/1.1\r\nHost: h\r\n\r\nraw",
        b"\r\n\r\nGET /lead HTTP/1.0\r\nConnection: keep-alive\r\n\r\n",
    ]
    write("inbound_stream", [bytes([c]) + r for r in requests for c in (0, 1, 7)])

    # upgrade_head: request heads and bare targets.
    heads = [
        b"GET /svc/ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        b"GET /x HTTP/1.0\r\nUpgrade: h2c\r\nConnection: upgrade\r\n\r\n",
    ]
    write("upgrade_head", heads + [t.encode() for t in targets[:100]])

    # websocket_response: response head, then what followed.
    responses = [
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n\x81\x02hi",
        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 4\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\nnope",
        b"HTTP/1.1 403 Forbidden\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nnope\r\n0\r\n\r\n",
        b"HTTP/1.0 500 Oops\r\n\r\nuntil close",
        b"HTTP/1.1 204 No Content\r\n\r\n",
    ]
    write("websocket_response", responses)


if __name__ == "__main__":
    main()
