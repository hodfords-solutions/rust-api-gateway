import type { Request, Response } from 'express';
import { McpAuthenticationMiddleware, type McpAuthenticationMiddlewareHandler } from '@hodfords/nestjs-api-gateway';
import { header } from '../protocol.js';

@McpAuthenticationMiddleware()
export class TestMcpAuthentication implements McpAuthenticationMiddlewareHandler {
    async authenticate(request: Request, response: Response): Promise<boolean> {
        const mode = header(request, 'x-test-mcp-auth') ?? 'allow';
        switch (mode) {
            case 'allow':
                return true;
            case 'deny':
                return false;
            case 'respond': {
                const status = Number(header(request, 'x-test-error-status') ?? '418');
                const body = header(request, 'x-test-error-body') ?? '{"message":"test mcp authentication response"}';
                response.status(status).type('application/json').send(body);
                return false;
            }
            default:
                throw new Error(`unknown x-test-mcp-auth mode: ${mode}`);
        }
    }
}
