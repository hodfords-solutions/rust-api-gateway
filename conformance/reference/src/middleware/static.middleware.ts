import type { IncomingMessage } from 'node:http';
import { ProxyValidation, type ProxyValidationHandler } from '@hodfords/nestjs-api-gateway';
import { isStaticRequest } from '../protocol.js';

@ProxyValidation()
export class TestStaticRequest implements ProxyValidationHandler {
    isStaticRequest(request: IncomingMessage): boolean {
        return isStaticRequest(request);
    }
}
