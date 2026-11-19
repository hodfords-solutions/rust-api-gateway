import { Module } from '@nestjs/common';
import { ScheduleModule } from '@nestjs/schedule';
import { ApiGatewayModule } from '@hodfords/nestjs-api-gateway';
import { config, redisTls } from './config.js';
import { resolveIdentity } from './protocol.js';
import { mcpOption } from './mcp-callbacks.js';
import { ConformanceController } from './conformance.controller.js';
import { TestProxyMiddleware } from './middleware/proxy.middleware.js';
import { TestWsMiddleware } from './middleware/ws.middleware.js';
import { TestStaticRequest } from './middleware/static.middleware.js';
import { TestMcpAuthentication } from './middleware/mcp.middleware.js';

const { keyResolver, ...throttler } = config.throttler;
const tls = redisTls();

@Module({
    imports: [
        ScheduleModule.forRoot(),
        ApiGatewayModule.forRoot({
            apiServices: config.apiServices,
            excludeHeaders: config.excludeHeaders,
            openApiSecurityKeys: config.openApiSecurityKeys,
            openApiSecurityApiKeys: config.openApiSecurityApiKeys,
            throttler: {
                ...throttler,
                ...(keyResolver === 'protocol' ? { keyResolver: resolveIdentity } : {})
            },
            restful: config.restful,
            scalarOptions: config.scalarOptions,
            redis: {
                host: config.redis.host,
                port: config.redis.port,
                db: config.redis.db,
                username: config.redis.username,
                password: config.redis.password,
                ...(tls ? { tls } : {})
            },
            pool: config.pool,
            bypassRoutePrefixes: config.bypassRoutePrefixes,
            // Not in the pinned reference's ApiGatewayOption before 12.5.0; spread so that
            // the option type does not reject it.
            ...({ websocketRoutePrefixes: config.websocketRoutePrefixes } as object),
            mcp: mcpOption(config.mcp)
        })
    ],
    controllers: [ConformanceController],
    providers: [TestProxyMiddleware, TestWsMiddleware, TestStaticRequest, TestMcpAuthentication]
})
export class AppModule {}
