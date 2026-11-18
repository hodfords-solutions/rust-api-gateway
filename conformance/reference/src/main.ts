import 'reflect-metadata';
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
    // No host argument, as in the reference sample: Node listens on `::` (dual stack).
    await app.listen(config.port);
}

bootstrap()
    .then(() => console.log(`conformance reference gateway listening on ${config.port}`))
    .catch((error) => {
        console.error(error);
        process.exit(1);
    });
