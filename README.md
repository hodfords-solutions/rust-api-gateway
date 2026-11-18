# rust-gateway

A Rust port of the [`@hodfords/nestjs-api-gateway`](https://www.npmjs.com/package/@hodfords/nestjs-api-gateway) library (the Reference Gateway, version 12.4.2). It is meant to replace that gateway in front of unchanged NestJS upstream services: it reads each service's OpenAPI document, builds routes from it, and forwards requests with middleware and Redis-backed rate limiting, reproducing the Reference Gateway's observable behaviour.

**Status: pre-parity.** No gateway behaviour is implemented yet. This repository currently holds the Parity Checklist and the conformance harness that every feature will be tested against.

## Layout

| Path | Contents |
|---|---|
| `crates/rust-gateway` | The library crate. |
| `crates/rust-gateway-server` | Example binary; also the Rust target of the Conformance Tests. |
| `conformance/` | The conformance harness: reference app, OpenAPI fixtures, mock upstream and scenario runner. See [conformance/README.md](conformance/README.md). |
| `docs/parity/` | The Parity Checklist: every Reference Gateway behaviour to reproduce, each tied to a test. |
| `docs/adr/` | Architecture decisions. |
| `CONTEXT.md` | Domain vocabulary. |

## Development

See [docs/development.md](docs/development.md) for running unit tests, the Conformance Tests against either gateway, and the working rules.

## License

[MIT](LICENSE)
