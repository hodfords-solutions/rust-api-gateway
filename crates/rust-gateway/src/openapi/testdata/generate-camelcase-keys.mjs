// Generates `camelcase-keys.json`: golden vectors for the Rust port of `camelcase-keys`
// (shallow, default options) in `crates/rust-gateway/src/openapi/camelcase.rs`.
//
// It runs the copy of `camelcase-keys` that the Reference Gateway itself resolves, from the
// conformance reference app.
//
// Usage, from the repository root, after `pnpm install --frozen-lockfile` in
// `conformance/reference`:
//
//     node crates/rust-gateway/src/openapi/testdata/generate-camelcase-keys.mjs
//
// Commit the output together with any change to this script.

import { createRequire } from 'node:module';
import { readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '../../../../..');
const gateway = realpathSync(join(root, 'conformance/reference/node_modules/@hodfords/nestjs-api-gateway'));
const require = createRequire(join(gateway, 'package.json'));
const entry = require.resolve('camelcase-keys');
const { default: camelcaseKeys } = await import(pathToFileURL(entry).href);
const version = JSON.parse(readFileSync(join(dirname(entry), 'package.json'), 'utf8')).version;

const keys = [
    // Keys real operation objects carry.
    'operationId',
    'summary',
    'description',
    'tags',
    'parameters',
    'requestBody',
    'responses',
    'callbacks',
    'deprecated',
    'security',
    'servers',
    'externalDocs',
    'x-rate-limits',
    'x-router-path',
    'x-api-mcp',
    'x-codeSamples',
    'x-code-samples',
    'x-internal',
    'x-oauth2-scopes',
    'x-v2',
    'x-api-v2-mcp',
    'x-foo_bar',
    'x_foo',
    'x__foo',
    'x--foo',
    'x-_-foo',
    'x-foo-',
    'x-',
    '-x',
    '_private',
    '__proto__',
    '$ref',
    '$$x',
    '_$x',
    '$_x',
    // Digits.
    'x-1',
    'x-1a',
    'x-a1',
    'x-a1b',
    'x-a1-b',
    'x-a12-b',
    'x-a12#b',
    'x-123',
    '123abc',
    'b2b_registration',
    'x-foo-123-bar',
    'foo2Bar',
    'foo2bar',
    'a1A',
    'x-1.5',
    // Upper case.
    'X-Rate-Limits',
    'XRateLimits',
    'x-HTTPMethod',
    'x-URL',
    'x-iOS',
    'A',
    'AB',
    'ABc',
    'FOOBar',
    'fooBAR',
    'fooBarBaz',
    'x-Foo-BAR-baz',
    'ID',
    'userID',
    'XMLHttpRequest',
    // Separators and white space.
    'foo.bar',
    'x.y.z',
    'foo bar',
    ' padded ',
    '_',
    '-',
    '.',
    ' ',
    'a',
    ' x',
    '﻿x',
    'x\u0085',
    'x- foo',
    'a\tb',
    // Numeric keys (kept as they are).
    '1',
    '0',
    '01',
    '1.5',
    '1e3',
    '1E+3',
    '0x10',
    '0X1f',
    '0o7',
    '0b101',
    ' 12 ',
    '-5',
    '+5',
    '.5',
    '5.',
    'Infinity',
    '-Infinity',
    'NaN',
    '1_000',
    '0x',
    '--5',
    '',
    '  ',
    // Non-ASCII.
    'x-émoji',
    'x-Émoji',
    'x-ß',
    'x-Σa',
    'x-İstanbul',
    'x-\u{1d400}b',
    'x-ǅ',
    'x-ⓐb',
    'x-Ⓐb',
    'x-ⅰb',
    'x-Ⅱb',
    'x-café-au-lait',
    'x-١٢',
];

const objects = [
    // A realistic operation, in the order @nestjs/swagger writes it.
    `{"operationId":"UsersController_createUser","summary":"Create","description":"Creates a user","parameters":[],"requestBody":{"required":true,"content":{}},"responses":{"201":{"description":""}},"tags":["users"],"security":[{"bearer":[]}],"x-rate-limits":[{"limit":1,"ttl":60,"status":201}],"x-router-path":"/v1/users","x-api-mcp":[{"description":"tool"}],"x-codeSamples":[]}`,
    // Collisions: the last value wins, at the first key's position.
    `{"x-foo":1,"xFoo":2,"x_foo":3,"other":4}`,
    // Integer-like keys come first, in ascending order.
    `{"b":1,"2":2,"a":3,"1":4,"x-1":5,"10":6,"4294967294":7,"4294967295":8,"-1":9,"01":10}`,
    // Duplicate keys in the JSON text.
    `{"a":1,"b":2,"a":3}`,
    // __proto__ is an own property after JSON.parse.
    `{"__proto__":1,"x":2}`,
    `{}`,
];

const vectors = {
    generator: 'crates/rust-gateway/src/openapi/testdata/generate-camelcase-keys.mjs',
    camelcaseKeysVersion: version,
    node: process.version,
    unicode: process.versions.unicode,
    keys: keys.map((key) => {
        const result = Object.keys(camelcaseKeys({ [key]: true }));
        return { input: key, output: result.length === 1 ? result[0] : null };
    }),
    objects: objects.map((json) => ({
        input: json,
        output: Object.entries(camelcaseKeys(JSON.parse(json))),
    })),
};

writeFileSync(join(here, 'camelcase-keys.json'), JSON.stringify(vectors, null, 1) + '\n');
console.log(`wrote ${keys.length} keys and ${objects.length} objects with camelcase-keys ${version}`);
