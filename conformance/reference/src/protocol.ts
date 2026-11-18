/**
 * The test middleware protocol: request headers that tell the conformance handlers what to do.
 * `rust-gateway-server` implements the same protocol. Documented in `conformance/README.md`.
 */
import { HttpException } from '@nestjs/common';
import type { IncomingMessage } from 'node:http';
import type { ProxyRequest, RouterDetail, ThrottlerKeyResolverContext } from '@hodfords/nestjs-api-gateway';

/** First value of a request header, or `undefined`. */
export function header(request: IncomingMessage, name: string): string | undefined {
    const value = request.headers[name];
    return Array.isArray(value) ? value[0] : value;
}

/** JSON with every non-ASCII character escaped, so it is a valid header value. */
function asciiJson(value: unknown): string {
    return JSON.stringify(value).replace(/[\u007f-￿]/g, (c) => '\\u' + c.charCodeAt(0).toString(16).padStart(4, '0'));
}

function describeRoute(route: RouterDetail): Record<string, unknown> {
    return {
        operationId: route.operationId ?? null,
        description: route.description,
        path: route.path,
        routerPath: route.routerPath,
        isBearerAuth: route.isBearerAuth,
        isApiKeyAuth: route.isApiKeyAuth,
        rateLimits: route.rateLimits,
        extraKeys: Object.keys(route.extra ?? {}).sort()
    };
}

/** Builds the error a handler throws for `x-test-middleware: throw`. */
function protocolError(request: IncomingMessage): HttpException {
    const status = Number(header(request, 'x-test-error-status') ?? '401');
    const rawBody = header(request, 'x-test-error-body') ?? '{"message":"test middleware error"}';
    let body: string | Record<string, unknown>;
    try {
        body = JSON.parse(rawBody);
    } catch {
        body = rawBody;
    }
    return new HttpException(body, status);
}

/**
 * Shared by the proxy middleware (with a Route) and the WebSocket middleware (without one).
 * Returns whether the request is allowed; throws for `throw` and `crash`.
 */
export function runMiddleware(request: IncomingMessage, proxyRequest: ProxyRequest, route?: RouterDetail): boolean {
    const user = header(request, 'x-test-auth-user');
    if (user !== undefined) {
        proxyRequest.addHeaders({ authUserId: user });
        (request as any).authUserId = user;
    }
    const extra = header(request, 'x-test-add-headers');
    if (extra !== undefined) {
        proxyRequest.addHeaders(JSON.parse(extra));
    }
    if (route && header(request, 'x-test-echo-route') === '1') {
        proxyRequest.addHeader('x-test-route', asciiJson(describeRoute(route)));
    }

    const mode = header(request, 'x-test-middleware') ?? 'allow';
    switch (mode) {
        case 'allow':
            return true;
        case 'deny':
            return false;
        case 'throw':
            throw protocolError(request);
        case 'crash':
            throw new Error('test middleware crash');
        default:
            throw new Error(`unknown x-test-middleware mode: ${mode}`);
    }
}

export function isStaticRequest(request: IncomingMessage): boolean {
    return header(request, 'x-test-static') === '1';
}

/** The Identity resolver installed when `throttler.keyResolver` is `"protocol"`. */
export function resolveIdentity({ request, ip }: ThrottlerKeyResolverContext): { key: string | null; authenticated: boolean } {
    if (header(request, 'x-test-identity-throw') === '1') {
        throw new Error('test identity resolver failure');
    }
    const authUserId = (request as any).authUserId as string | undefined;
    let key: string | null;
    let fromUser = false;
    if (header(request, 'x-test-identity-none') === '1') {
        key = null;
    } else if (header(request, 'x-test-identity') !== undefined) {
        key = header(request, 'x-test-identity')!;
    } else if (authUserId) {
        key = `user:${authUserId}`;
        fromUser = true;
    } else {
        key = ip;
    }
    const authenticatedHeader = header(request, 'x-test-identity-authenticated');
    const authenticated = authenticatedHeader !== undefined ? authenticatedHeader === 'true' : fromUser;
    return { key, authenticated };
}
