/**
 * The benchmark middleware set (EX-05). `crates/rust-gateway-server/src/bench.rs` implements
 * the same rules for the Rust gateway; keep the two identical.
 *
 * It stands for what production middleware is assumed to do (an assumption nobody has
 * confirmed yet):
 *
 * - Proxy middleware: a request with `Authorization: Bearer <token>` must carry an HS256 JWT
 *   signed with `BENCH_JWT_SECRET`, unexpired and with a string `sub`; then `authUserId`
 *   (forwarded as `auth-user-id`) is set to `sub`. A bad token is rejected with Nest's
 *   `UnauthorizedException` (401 `{"message":"Unauthorized","statusCode":401}`). A request
 *   without a token is rejected the same way when its Route requires bearer authentication,
 *   and passes anonymously otherwise.
 * - Identity resolver: the user id, authenticated, when the middleware set one; otherwise the
 *   client address, not authenticated.
 * - WebSocket middleware: the same token check, without a Route.
 * - Static requests and MCP authentication: as the reference sample.
 */
import type { IncomingMessage } from 'node:http';
import { UnauthorizedException } from '@nestjs/common';
import jwt from 'jsonwebtoken';
import {
    McpAuthenticationMiddleware,
    type McpAuthenticationMiddlewareHandler,
    ProxyMiddleware,
    type ProxyMiddlewareHandler,
    ProxyRequest,
    ProxyValidation,
    type ProxyValidationHandler,
    type RouterDetail,
    type ThrottlerKeyResolverContext,
    WsProxyMiddleware,
    type WsProxyMiddlewareHandler
} from '@hodfords/nestjs-api-gateway';

const secret = process.env.BENCH_JWT_SECRET;
if (!secret) {
    throw new Error('the benchmark middleware needs BENCH_JWT_SECRET');
}

/** The token of an `Authorization: Bearer <token>` header. */
function bearer(request: IncomingMessage): string | undefined {
    const value = request.headers.authorization;
    return value?.startsWith('Bearer ') ? value.slice('Bearer '.length) : undefined;
}

/** The token's `sub` when the token is valid and `sub` is a non-empty string. */
function subject(token: string): string | undefined {
    try {
        const payload = jwt.verify(token, secret!, { algorithms: ['HS256'] });
        const sub = typeof payload === 'object' ? payload.sub : undefined;
        return typeof sub === 'string' && sub !== '' ? sub : undefined;
    } catch {
        return undefined;
    }
}

function authenticate(request: IncomingMessage, proxyRequest: ProxyRequest, requiresBearer: boolean): boolean {
    const token = bearer(request);
    if (token !== undefined) {
        const user = subject(token);
        if (user === undefined) {
            throw new UnauthorizedException();
        }
        proxyRequest.addHeader('authUserId', user);
        (request as any).authUserId = user;
        return true;
    }
    if (requiresBearer) {
        throw new UnauthorizedException();
    }
    return true;
}

@ProxyMiddleware()
export class JwtMiddleware implements ProxyMiddlewareHandler {
    async handle(routerDetail: RouterDetail, request: IncomingMessage, proxyRequest: ProxyRequest): Promise<boolean> {
        return authenticate(request, proxyRequest, routerDetail.isBearerAuth);
    }
}

@WsProxyMiddleware()
export class JwtWsMiddleware implements WsProxyMiddlewareHandler {
    async handle(request: IncomingMessage, proxyRequest: ProxyRequest): Promise<boolean> {
        return authenticate(request, proxyRequest, false);
    }
}

/** URLs containing `/images/` or `/statics/` are Static Requests (the reference sample). */
@ProxyValidation()
export class BenchStaticRequest implements ProxyValidationHandler {
    isStaticRequest(request: IncomingMessage): boolean {
        const url = request.url ?? '';
        return url.includes('/images/') || url.includes('/statics/');
    }
}

/** Accepts every MCP request (the reference sample). */
@McpAuthenticationMiddleware()
export class BenchMcpAuthentication implements McpAuthenticationMiddlewareHandler {
    async authenticate(): Promise<boolean> {
        return true;
    }
}

/** The user id when the middleware authenticated the request, else the client address. */
export function resolveIdentity({ request, ip }: ThrottlerKeyResolverContext): { key: string; authenticated: boolean } {
    const authUserId = (request as any).authUserId as string | undefined;
    return authUserId ? { key: authUserId, authenticated: true } : { key: ip, authenticated: false };
}
