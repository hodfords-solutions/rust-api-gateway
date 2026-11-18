import { Controller, Get, Module } from '@nestjs/common';
import { ScheduleModule } from '@nestjs/schedule';
import { ApiGatewayModule, OpenApiService } from '@hodfords/nestjs-api-gateway';
import { config, redisTls } from './config.js';
import {
    BenchStaticRequest,
    BenchMcpAuthentication,
    JwtMiddleware,
    JwtWsMiddleware,
    resolveIdentity
} from './middleware.js';

/** Answers once every service's first document load has settled (CFG-16). */
@Controller('__bench')
export class BenchController {
    constructor(private readonly openApiService: OpenApiService) {}

    @Get('ready')
    async ready(): Promise<{ services: string[] }> {
        await this.openApiService.ready;
        return { services: Object.keys(this.openApiService.apiDocs) };
    }
}

// The file's `keyResolver` is ignored: the benchmark always installs its own resolver.
const { keyResolver: _ignored, ...throttler } = config.throttler;
const tls = redisTls();

@Module({
    imports: [
        ScheduleModule.forRoot(),
        ApiGatewayModule.forRoot({
            apiServices: config.apiServices,
            excludeHeaders: config.excludeHeaders,
            openApiSecurityKeys: config.openApiSecurityKeys,
            openApiSecurityApiKeys: config.openApiSecurityApiKeys,
            throttler: { ...throttler, keyResolver: resolveIdentity },
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
            ...(config.mcp ? { mcp: config.mcp } : {})
        })
    ],
    controllers: [BenchController],
    providers: [JwtMiddleware, JwtWsMiddleware, BenchStaticRequest, BenchMcpAuthentication]
})
export class AppModule {}
