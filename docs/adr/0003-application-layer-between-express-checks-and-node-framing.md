---
status: accepted
---

# The application's layer sits between Express's checks and Node's framing

An application replacing a NestJS gateway app needs what that app adds with Express middleware: CORS on every route, preflights answered at the gateway, and default response headers on proxied responses too (REQ-27). `GatewayBuilder::layer` takes any tower layer and applies it inside the router that `Gateway::serve` serves: outside Express's own checks (malformed percent-encoding, REQ-26; the asterisk-form target) and the proxy, inside the headers Node completes every response with (`X-Powered-By` when on, `Content-Length`, `Date`, `Connection`, `Keep-Alive`, `Transfer-Encoding`) and the immediate `100 Continue`. WebSocket upgrades taken over by `serve` never reach it. That is where Express middleware sits in the reference's host application, and it is hard to move later: an application's layer would start or stop seeing some responses, or start setting headers Node now owns.

## Considered Options

- **Outermost, around Node's framing**: a layer could rewrite everything, but what it answers itself (a CORS preflight) would go out with no `Date`, `Connection` or `Keep-Alive`, and headers it adds would land after Node's, unlike Express.
- **Inside Express's checks, around the proxy only**: the 400 of REQ-26 and the 404 of `*` would go out without the application's headers, so a browser could not read them under CORS.
- **A `Fn(Router) -> Router` transform**: more general, but it could add routes or a fallback that break the proxy's catch-all (CFG-18). A layer covers every case the consumer needs.
- **Letting the layer see WebSocket upgrades**: the upgrade is taken over before hyper so the WebSocket proxy can write its exact bytes (WS-06, WS-15, WS-16); routing it through a tower layer would lose that. Node does not pass upgrades through Express middleware either. `WsProxyMiddleware` is the hook for upgrades.

## Consequences

- A layer should not set the headers Node owns; they would be duplicated or overridden.
- With no layer registered, the router is the one it was before (`gateway_layer::req_27_identity_layer_changes_nothing`), and the Conformance Tests do not change.
- The answers to rejected upgrades (Deviation D1) are written by the WebSocket proxy and carry none of the layer's headers.
