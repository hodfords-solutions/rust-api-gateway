/**
 * Writes the OpenAPI fixtures used by the Conformance Tests.
 *
 *     node dist/generate.js <output-dir>
 *
 * Each fixture is produced by `@nestjs/swagger` from a real NestJS module whose operations carry
 * `@hodfords/nestjs-api-gateway-client` decorators. Two edits are made afterwards, because no
 * decorator can express them; both are listed in `conformance/README.md`:
 *   - operations marked `x-fixture-drop-operation-id` lose their `operationId` (and the marker);
 *   - `invalid-path-item.json` gets a path-level `parameters` key (OAS-17, Deviation D3).
 */
import 'reflect-metadata';
import { writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { NestFactory } from '@nestjs/core';
import type { NestExpressApplication } from '@nestjs/platform-express';
import { DocumentBuilder, SwaggerModule, type OpenAPIObject } from '@nestjs/swagger';
import { DROP_OPERATION_ID, UsersModule, UsersV2Module } from './users.js';
import { DefaultModule } from './default.js';

async function document(module: unknown, title: string): Promise<OpenAPIObject> {
    const app = await NestFactory.create<NestExpressApplication>(module as any, { logger: false });
    const config = new DocumentBuilder()
        .setTitle(title)
        .setDescription(`Conformance fixture ${title}`)
        .setVersion('1.0')
        .addBearerAuth()
        .addSecurity('auth-user-id', { type: 'apiKey', in: 'header', name: 'auth-user-id' })
        .addApiKey({ type: 'apiKey', in: 'header', name: 'x-api-key' }, 'x-api-key')
        .build();
    const doc = SwaggerModule.createDocument(app, config);
    await app.close();
    return dropMarkedOperationIds(JSON.parse(JSON.stringify(doc)));
}

function dropMarkedOperationIds(doc: any): any {
    for (const pathItem of Object.values<any>(doc.paths)) {
        for (const operation of Object.values<any>(pathItem)) {
            if (operation && typeof operation === 'object' && operation[DROP_OPERATION_ID]) {
                delete operation.operationId;
                delete operation[DROP_OPERATION_ID];
            }
        }
    }
    return doc;
}

async function main(): Promise<void> {
    const out = resolve(process.argv[2] ?? '../fixtures');
    const fixtures: Record<string, any> = {
        'users-service.json': await document(UsersModule, 'users-service'),
        'users-service-v2.json': await document(UsersV2Module, 'users-service'),
        'default-service.json': await document(DefaultModule, 'default-service')
    };

    const invalid = await document(DefaultModule, 'invalid-path-item');
    const [firstPath] = Object.keys(invalid.paths);
    invalid.paths[firstPath] = {
        parameters: [{ name: 'x-trace-id', in: 'header', required: false, schema: { type: 'string' } }],
        ...invalid.paths[firstPath]
    };
    fixtures['invalid-path-item.json'] = invalid;

    for (const [name, doc] of Object.entries(fixtures)) {
        writeFileSync(join(out, name), JSON.stringify(doc, null, 2) + '\n');
        console.log(`wrote ${join(out, name)}`);
    }
}

main().catch((error) => {
    console.error(error);
    process.exit(1);
});
