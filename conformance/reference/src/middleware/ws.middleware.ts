import type { IncomingMessage } from 'node:http';
import { ProxyRequest, WsProxyMiddleware, type WsProxyMiddlewareHandler } from '@hodfords/nestjs-api-gateway';
import { runMiddleware } from '../protocol.js';

@WsProxyMiddleware()
export class TestWsMiddleware implements WsProxyMiddlewareHandler {
    async handle(request: IncomingMessage, proxyRequest: ProxyRequest): Promise<boolean> {
        return runMiddleware(request, proxyRequest);
    }
}
