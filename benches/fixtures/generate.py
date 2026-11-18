#!/usr/bin/env python3
"""Writes the benchmark's fixtures: bench-service.json, the OpenAPI document of the benchmark
Upstream Service, and order-4k.json, the JSON body of the POST scenario.

A realistically sized NestJS-style document (282 operations on 40 resources), shaped like
what @nestjs/swagger and @hodfords/nestjs-api-gateway-client emit: bearer security on most
operations, some `x-rate-limits`, path parameters, query parameters and JSON bodies.

The end-to-end scenarios use three of its Routes, placed in the middle of the document so a
lookup does a realistic amount of matching:

    GET  /v1/orders/{id}             (small JSON, the headline)
    POST /v1/orders                  (JSON body; one Route limit, so one more Redis round trip)
    GET  /v1/reports/{id}/export     (about 1 MB)

The criterion benchmarks in crates/rust-gateway/benches use the same file.
Deterministic: run `python3 benches/fixtures/generate.py` and commit the output.
"""
import json
import os

RESOURCES = [
    "accounts", "addresses", "alerts", "articles", "audits", "baskets", "billing-plans",
    "campaigns", "carriers", "categories", "comments", "contracts", "coupons", "customers",
    "deliveries", "devices", "discounts", "documents", "employees", "events",
    # the scenarios' resources sit in the middle
    "orders", "reports",
    "invoices", "jobs", "labels", "locations", "messages", "notifications", "payments",
    "permissions", "products", "promotions", "refunds", "reviews", "roles", "sessions",
    "shipments", "subscriptions", "tickets", "warehouses",
]
SUB = ["items", "notes", "history"]


def camel(name):
    parts = name.split("-")
    return parts[0] + "".join(p.title() for p in parts[1:])


def controller(resource):
    return "".join(p.title() for p in resource.split("-")) + "Controller"


def op(resource, action, summary, *, params=None, body=None, bearer=True, limits=None, status="200"):
    o = {
        "operationId": f"{controller(resource)}_{action}",
        "parameters": params or [],
        "responses": {status: {"description": ""}},
        "summary": summary,
        "tags": [resource],
    }
    if body:
        o["requestBody"] = {
            "required": True,
            "content": {"application/json": {"schema": {"$ref": f"#/components/schemas/{body}"}}},
        }
    if bearer:
        o["security"] = [{"bearer": []}]
    if limits:
        o["x-rate-limits"] = limits
    return o


def path_param(name):
    return {"name": name, "required": True, "in": "path", "schema": {"type": "string"}}


def query_params():
    return [
        {"name": "page", "required": False, "in": "query", "schema": {"type": "number"}},
        {"name": "perPage", "required": False, "in": "query", "schema": {"type": "number"}},
        {"name": "sort", "required": False, "in": "query", "schema": {"enum": ["asc", "desc"], "type": "string"}},
    ]


# Never reached in a benchmark: high enough that no run can fill the Bucket.
BIG_LIMIT = [{"limit": 1000000000, "ttl": 60}]


def main():
    paths = {}
    schemas = {}
    for i, r in enumerate(RESOURCES):
        dto = "Create" + controller(r).removesuffix("Controller") + "Dto"
        schemas[dto] = {
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "description": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "metadata": {"type": "object"},
            },
            "required": ["name"],
        }
        # Every fifth resource is public, as login, catalogue and health endpoints are.
        bearer = i % 5 != 0
        create_limits = BIG_LIMIT if r == "orders" or i % 4 == 0 else None
        paths[f"/v1/{r}"] = {
            "get": op(r, "list", f"List {r}", params=query_params(), bearer=bearer),
            "post": op(r, "create", f"Create one of {r}", body=dto, bearer=bearer, limits=create_limits, status="201"),
        }
        paths[f"/v1/{r}/{{id}}"] = {
            "get": op(r, "get", f"Get one of {r}", params=[path_param("id")], bearer=bearer),
            "patch": op(r, "update", f"Update one of {r}", params=[path_param("id")], body=dto, bearer=bearer),
            "delete": op(r, "remove", f"Delete one of {r}", params=[path_param("id")], bearer=bearer),
        }
        sub = SUB[i % len(SUB)]
        paths[f"/v1/{r}/{{id}}/{sub}"] = {
            "get": op(r, f"list{sub.title()}", f"List the {sub} of one of {r}", params=[path_param("id")] + query_params(), bearer=bearer),
        }
        if r == "reports":
            paths["/v1/reports/{id}/export"] = {
                "get": op(r, "export", "Export a report", params=[path_param("id")], bearer=True),
            }
        if i % 2 == 0:
            paths[f"/v1/{r}/{{id}}/{sub}/{{{camel(sub)}Id}}"] = {
                "get": op(r, f"get{sub.title()}", f"Get one of the {sub}", params=[path_param("id"), path_param(f"{camel(sub)}Id")], bearer=bearer),
                "delete": op(r, f"remove{sub.title()}", f"Delete one of the {sub}", params=[path_param("id"), path_param(f"{camel(sub)}Id")], bearer=bearer),
            }
    paths["/health"] = {"get": op("health", "check", "Health check", bearer=False)}
    document = {
        "openapi": "3.0.0",
        "paths": paths,
        "info": {"title": "bench-service", "description": "Benchmark upstream", "version": "1.0", "contact": {}},
        "tags": [],
        "servers": [],
        "components": {
            "securitySchemes": {
                "bearer": {"scheme": "bearer", "bearerFormat": "JWT", "type": "http"},
            },
            "schemas": schemas,
        },
    }
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "bench-service.json")
    with open(out, "w") as f:
        json.dump(document, f, indent=2)
        f.write("\n")
    count = sum(len(v) for v in paths.values())
    print(f"wrote {out}: {len(paths)} paths, {count} operations")

    # A few kilobytes of JSON, shaped like an order with line items.
    order = {
        "customerId": "cus_000042",
        "currency": "EUR",
        "shippingAddress": {"name": "Ada Lovelace", "line1": "12 Example Street", "city": "Hanoi", "postcode": "100000", "country": "VN"},
        "items": [
            {"sku": f"SKU-{n:05d}", "name": f"Benchmark item number {n}", "quantity": n % 3 + 1, "unitPrice": 10 + n * 1.25, "tags": ["bench", "e2e"]}
            for n in range(36)
        ],
        "note": "Leave at the front desk.",
    }
    body = json.dumps(order, separators=(",", ":")) + "\n"
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "order-4k.json")
    with open(out, "w") as f:
        f.write(body)
    print(f"wrote {out}: {len(body)} bytes")


if __name__ == "__main__":
    main()
