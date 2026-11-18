import type { IncomingMessage } from 'node:http';
import { ProxyMiddleware, type ProxyMiddlewareHandler, ProxyRequest, type RouterDetail } from '@hodfords/nestjs-api-gateway';
import { runMiddleware } from '../protocol.js';

@ProxyMiddleware()
export class TestProxyMiddleware implements ProxyMiddlewareHandler {
    async handle(routerDetail: RouterDetail, request: IncomingMessage, proxyRequest: ProxyRequest): Promise<boolean> {
        return runMiddleware(request, proxyRequest, routerDetail);
    }
}
