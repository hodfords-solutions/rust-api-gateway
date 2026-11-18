# rust-gateway

A Rust port of the `@hodfords/nestjs-api-gateway` library. It exists to replace that gateway in front of unchanged upstream services.

## Language

### Porting

**Reference Gateway**:
The NestJS implementation this project is ported from, whose observable behaviour defines what is correct.
_Avoid_: original, old gateway, legacy gateway, Node gateway

**External Contract**:
Everything a client or an upstream service can observe from the gateway over the network: requests it forwards, responses and errors it returns, and the documents and endpoints it serves. State the gateway keeps for itself is not part of it.
_Avoid_: API, interface, wire format

**Extension Contract**:
The custom fields that upstream services publish in their OpenAPI documents to instruct the gateway, and the meaning the gateway gives them.
_Avoid_: client decorators, metadata, annotations

**Deviation**:
A deliberate, individually approved difference between this gateway's External Contract and the Reference Gateway's.
_Avoid_: fix, improvement, incompatibility

**Parity Checklist**:
The per-feature list of Reference Gateway behaviours this gateway must reproduce, each tied to at least one test.
_Avoid_: feature list, TODO, spec

**Conformance Test**:
A black-box scenario run against both the Reference Gateway and this gateway, passing only when both respond the same way.
_Avoid_: integration test, e2e test, differential test

### Routing

**Upstream Service**:
A backend that the gateway forwards requests to and reads an OpenAPI document from.
_Avoid_: api service, server, sub-service, backend

**Route**:
One operation taken from an Upstream Service's OpenAPI document, identified by method and path pattern.
_Avoid_: router, router detail, endpoint

**Service Prefix**:
The leading path segment that assigns a request to an Upstream Service and is removed before forwarding.
_Avoid_: service name, server name

**Direct Prefix**:
A leading path segment owned by an Upstream Service that assigns a request to it and is kept when forwarding.
_Avoid_: passthrough prefix

**Default Service**:
The Upstream Service that receives requests no Service Prefix or Direct Prefix claims.
_Avoid_: fallback server, default server

### Request classes

**Routed Request**:
A request matched to a Route, and therefore subject to middleware and rate limiting.
_Avoid_: normal request

**Static Request**:
A request the application declares exempt from Route matching, middleware and rate limiting.
_Avoid_: validated request, asset request

**Bypass Request**:
A request under a configured path prefix that is forwarded without Route matching, middleware or rate limiting.
_Avoid_: whitelisted request

### Rate limiting

**Identity**:
The key a request's rate limits are counted against, and whether that requester is authenticated.
_Avoid_: user, client key, throttler key

**Bucket**:
One rate-limit counter. The four kinds are Global IP, Anonymous IP, Custom and Route.
_Avoid_: limiter, counter, throttle
