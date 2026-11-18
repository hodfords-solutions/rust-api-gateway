import 'reflect-metadata';
import type { Server } from 'node:http';
import { NestFactory } from '@nestjs/core';
import type { NestExpressApplication } from '@nestjs/platform-express';
import { AppModule } from './app.module.js';
import { config } from './config.js';

/** Mirrors `src/main.ts` of the Reference Gateway repository. */
async function bootstrap(): Promise<void> {
    const app = await NestFactory.create<NestExpressApplication>(AppModule, {
        bodyParser: false
    });
    app.setViewEngine('hbs');
    app.set('trust proxy', config.trustProxy ?? ['loopback', 'linklocal', 'uniquelocal']);
    // Harness field: `http.Server` timeouts (REQ-28, REQ-29). Set before `listen`, which
    // starts the connections check with `connectionsCheckingInterval`.
    const server = app.getHttpServer() as Server;
    const timeouts = config.server ?? {};
    if (timeouts.headersTimeout !== undefined) server.headersTimeout = timeouts.headersTimeout;
    if (timeouts.requestTimeout !== undefined) server.requestTimeout = timeouts.requestTimeout;
    if (timeouts.keepAliveTimeout !== undefined) server.keepAliveTimeout = timeouts.keepAliveTimeout;
    if (timeouts.keepAliveTimeoutBuffer !== undefined) {
        (server as Server & { keepAliveTimeoutBuffer: number }).keepAliveTimeoutBuffer = timeouts.keepAliveTimeoutBuffer;
    }
    if (timeouts.connectionsCheckingInterval !== undefined) {
        (server as Server & { connectionsCheckingInterval: number }).connectionsCheckingInterval =
            timeouts.connectionsCheckingInterval;
    }
    // No host argument, as in the reference sample: Node listens on `::` (dual stack).
    await app.listen(config.port);
}

bootstrap()
    .then(() => console.log(`conformance reference gateway listening on ${config.port}`))
    .catch((error) => {
        console.error(error);
        process.exit(1);
    });
