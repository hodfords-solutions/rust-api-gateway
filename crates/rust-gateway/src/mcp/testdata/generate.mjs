// Golden vectors for `api-gateway`'s `mcp` module, produced by the Reference Gateway's own
// MCP code (`McpToolRegistryService` and `McpToolExecutorService` from
// `@hodfords/nestjs-api-gateway`, loaded from `conformance/reference/node_modules`).
//
// Run from the repository root after installing the reference app:
//
//     (cd conformance/reference && pnpm install --frozen-lockfile)
//     node crates/rust-gateway/src/mcp/testdata/generate.mjs
//
// Outputs (commit them):
//   testdata/registry.json  MCP-06 to MCP-12: documents -> tools and warnings, as the
//                           reference builds them and with Deviation D8 applied
//   testdata/call.json      MCP-17, MCP-18: tool + arguments + middleware headers -> the
//                           undici request options the reference builds
//
// Deviation D8 is produced by running the same reference code with one change: every call
// of `resolveSchema` receives a copy of its caller's set of references, so a reference is only
// treated as a cycle when it is one of its own ancestors.

import { createRequire } from 'node:module';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '../../../../..');
const reference = createRequire(join(root, 'conformance/reference/package.json'));
const gatewayDir = dirname(reference.resolve('@hodfords/nestjs-api-gateway'));
const load = (path) => import(pathToFileURL(join(gatewayDir, path)).href);

const { McpToolRegistryService } = await load('mcp/services/mcp-tool-registry.service.js');
const { McpToolExecutorService } = await load('mcp/services/mcp-tool-executor.service.js');
const { ProxyRequest } = await load('restful/models/proxy-request.model.js');

const versions = {
    node: process.version,
    '@hodfords/nestjs-api-gateway': JSON.parse(readFileSync(join(gatewayDir, 'package.json'), 'utf8')).version
};

const METHODS = ['get', 'post', 'delete', 'patch', 'put', 'options', 'head', 'search'];

/** The router the reference's OpenApiService builds, reduced to what the registry reads. */
function router(document) {
    const result = {};
    for (const method of METHODS) {
        result[method] = [];
        for (const path of Object.keys(document.paths ?? {})) {
            const operation = document.paths[path]?.[method];
            if (operation) {
                result[method].push({
                    operationId: operation.operationId,
                    description: operation.description ?? '',
                    path
                });
            }
        }
    }
    return result;
}

function registry(services, d8) {
    const openApiService = {
        apiDocs: Object.fromEntries(Object.entries(services).map(([name, doc]) => [name, { router: router(doc) }])),
        originDocs: services,
        ready: Promise.resolve()
    };
    const service = new McpToolRegistryService(openApiService, { enabled: true }, { apiServices: [] });
    const warnings = [];
    service.logger = { warn: (message) => warnings.push(message), log: () => {} };
    if (d8) {
        const original = service.resolveSchema.bind(service);
        service.resolveSchema = (doc, schema, refs = new Set()) => original(doc, schema, new Set(refs));
    }
    service.refreshTools();
    const tools = service.getTools().map((tool) => ({
        name: tool.name,
        description: tool.description,
        serviceName: tool.serviceName,
        httpMethod: tool.httpMethod,
        path: tool.path,
        inputSchema: tool.inputSchema
    }));
    return { tools, warnings };
}

const fixture = (name) => JSON.parse(readFileSync(join(root, 'conformance/fixtures', name), 'utf8'));
const info = { title: 't', version: '1' };
const ok = { 200: { description: '' } };
const param = (location, name, schema, extra = {}) => ({ in: location, name, schema, ...extra });
const op = (extra) => ({ responses: ok, ...extra });

const types = {
    openapi: '3.0.0',
    info,
    paths: {
        '/types': {
            get: op({
                operationId: 'types',
                parameters: [
                    param('query', 'plain', { type: 'string' }),
                    param('query', 'upper', { type: 'STRING' }),
                    param('query', 'mixed', { type: 'Integer' }),
                    param('query', 'ctor', { type: 'function String() { [native code] }' }),
                    param('query', 'ctorNumber', { type: 'function Number() { [native code] }' }),
                    param('query', 'ctorBigInt', { type: 'function BigInt() { [native code] }' }),
                    param('query', 'ctorBoolean', { type: 'function Boolean() { [native code] }' }),
                    param('query', 'ctorArray', { type: 'function Array() { [native code] }' }),
                    param('query', 'ctorObject', { type: 'function Object() { [native code] }' }),
                    param('query', 'ctorDate', { type: 'function Date() { [native code] }' }),
                    param('query', 'ctorAsync', { type: '  async function Number() {}' }),
                    param('query', 'ctorGenerator', { type: 'function* Boolean() {}' }),
                    param('query', 'ctorUnknown', { type: 'function Whatever() { [native code] }' }),
                    param('query', 'ctorLower', { type: 'function string() {}' }),
                    param('query', 'ctorNoSpace', { type: 'functionString() {}' }),
                    param('query', 'class', { type: 'class String {}' }),
                    param('query', 'list', { type: ['String', 'null'] }),
                    param('query', 'listMixed', { type: ['function Number() { }', 'bogus', 'BOOLEAN'] }),
                    param('query', 'listBad', { type: ['bogus'] }),
                    param('query', 'listEmpty', { type: [] }),
                    param('query', 'listNested', { type: [['string']] }),
                    param('query', 'bogus', { type: 'bogus' }),
                    param('query', 'number', { type: 42 }),
                    param('query', 'bool', { type: true }),
                    param('query', 'object', { type: {} }),
                    param('query', 'null', { type: null }),
                    param('query', 'noType', { format: 'uuid' }),
                    { in: 'query', name: 'noSchema' },
                    param('query', 'withEnum', { type: 'string', enum: ['a', 'b'] }),
                    param('query', 'emptyEnum', { type: 'string', enum: [] }),
                    param('query', 'stringEnum', { type: 'string', enum: 'abc' }),
                    param('query', 'zeroEnum', { type: 'number', enum: 0 }),
                    param('query', 'described', { type: 'string' }, { description: 'A thing' }),
                    param('query', 'emptyDescription', { type: 'string' }, { description: '' }),
                    param('path', 'req', { type: 'string' }, { required: true }),
                    param('header', 'reqTruthy', { type: 'string' }, { required: 'yes' }),
                    param('header', 'reqFalse', { type: 'string' }, { required: false }),
                    param('cookie', 'cookie', { type: 'string' }),
                    { name: 'noIn', schema: { type: 'string' } },
                    { in: 'query', schema: { type: 'string' } },
                    { $ref: '#/components/parameters/Shared' },
                    param('path', 'req', { type: 'number' }, { required: true, description: 'again' })
                ]
            })
        },
        '/no-parameters': { get: op({ operationId: 'noParameters' }) },
        '/empty-parameters': { get: op({ operationId: 'emptyParameters', parameters: [] }) }
    },
    components: { parameters: { Shared: { in: 'query', name: 'shared', schema: { type: 'string' } } } }
};

const bodies = {
    openapi: '3.0.0',
    info,
    paths: {
        '/required': {
            post: op({
                operationId: 'required',
                requestBody: {
                    required: true,
                    description: 'Payload',
                    content: { 'application/json': { schema: { $ref: '#/components/schemas/Person' } } }
                }
            })
        },
        '/optional': {
            post: op({
                operationId: 'optional',
                requestBody: { content: { 'application/json': { schema: { type: 'object' } } } }
            })
        },
        '/array': {
            put: op({
                operationId: 'array',
                requestBody: {
                    description: '',
                    content: { 'application/json': { schema: { type: 'array', items: { $ref: '#/components/schemas/Tag' } } } }
                }
            })
        },
        '/described-schema': {
            patch: op({
                operationId: 'describedSchema',
                requestBody: {
                    description: 'outer',
                    content: { 'application/json': { schema: { description: 'inner', properties: { a: { type: 'string' } } } } }
                }
            })
        },
        '/no-type': {
            post: op({
                operationId: 'noType',
                requestBody: { content: { 'application/json': { schema: { properties: { a: { type: 'Number' } } } } } }
            })
        },
        '/form-only': {
            post: op({
                operationId: 'formOnly',
                requestBody: { required: true, content: { 'application/x-www-form-urlencoded': { schema: { type: 'object' } } } }
            })
        },
        '/json-charset': {
            post: op({
                operationId: 'jsonCharset',
                requestBody: { content: { 'application/json; charset=utf-8': { schema: { type: 'object' } } } }
            })
        },
        '/no-schema': {
            post: op({ operationId: 'noSchema', requestBody: { required: true, content: { 'application/json': {} } } })
        },
        '/no-content': { post: op({ operationId: 'noContent', requestBody: { required: true } }) },
        '/unresolvable': {
            post: op({
                operationId: 'unresolvable',
                requestBody: { content: { 'application/json': { schema: { $ref: '#/components/schemas/Missing' } } } }
            })
        },
        '/with-params': {
            post: op({
                operationId: 'withParams',
                parameters: [param('path', 'id', { type: 'string' }, { required: true })],
                requestBody: { required: true, content: { 'application/json': { schema: { type: 'string', enum: ['x'] } } } }
            })
        },
        '/body-param-clash': {
            post: op({
                operationId: 'bodyParamClash',
                parameters: [param('body', '', { type: 'number' })],
                requestBody: { content: { 'application/json': { schema: { type: 'boolean' } } } }
            })
        },
        '/get-with-body': {
            get: op({
                operationId: 'getWithBody',
                requestBody: { content: { 'application/json': { schema: { type: 'object' } } } }
            })
        }
    },
    components: {
        schemas: {
            Person: {
                type: 'object',
                required: ['name'],
                properties: {
                    name: { type: 'string', description: 'Name' },
                    home: { $ref: '#/components/schemas/Address' },
                    work: { $ref: '#/components/schemas/Address' },
                    tags: { type: 'array', items: { $ref: '#/components/schemas/Tag' } },
                    friend: { $ref: '#/components/schemas/Person' },
                    friends: { type: 'array', items: { $ref: '#/components/schemas/Person' } },
                    choice: { oneOf: [{ $ref: '#/components/schemas/Tag' }, { type: 'string' }] },
                    all: { allOf: [{ $ref: '#/components/schemas/Address' }] },
                    map: { type: 'object', additionalProperties: { $ref: '#/components/schemas/Tag' } }
                }
            },
            Address: {
                type: 'object',
                properties: { street: { type: 'String' }, geo: { $ref: '#/components/schemas/Geo' } }
            },
            Geo: { type: 'object', properties: { lat: { type: 'number' }, lng: { type: 'number' } } },
            Tag: { type: 'string', enum: ['a', 'b'] }
        }
    }
};

const refs = {
    openapi: '3.0.0',
    info,
    paths: {
        '/refs': {
            post: op({
                operationId: 'refs',
                requestBody: {
                    content: {
                        'application/json': {
                            schema: {
                                type: 'object',
                                properties: {
                                    siblings1: { $ref: '#/components/schemas/Leaf' },
                                    siblings2: { $ref: '#/components/schemas/Leaf' },
                                    chain: { $ref: '#/components/schemas/A' },
                                    direct: { $ref: '#/components/schemas/C' },
                                    selfCycle: { $ref: '#/components/schemas/Self' },
                                    mutual: { $ref: '#/components/schemas/Ping' },
                                    withSiblings: { $ref: '#/components/schemas/Leaf', description: 'ignored', type: 'number' },
                                    toString: { $ref: '#/components/schemas/AString' },
                                    toNumber: { $ref: '#/components/schemas/ANumber' },
                                    toArray: { $ref: '#/components/schemas/AnArray' },
                                    toTrue: { $ref: '#/components/schemas/ATrue' },
                                    toNull: { $ref: '#/components/schemas/ANull' },
                                    hash: { $ref: '#' },
                                    hashSlash: { $ref: '#/' },
                                    external: { $ref: 'other.json#/components/schemas/Leaf' },
                                    pointerEscape: { $ref: '#/components/schemas/A~1B' },
                                    encoded: { $ref: '#/components/schemas/With%20Space' },
                                    spaced: { $ref: '#/components/schemas/With Space' },
                                    deep: { $ref: '#/components/schemas/Leaf/properties/x' },
                                    emptyRef: { $ref: '' },
                                    stringProperty: 'abc',
                                    unicodeString: 'hé',
                                    arrayProperty: [1, { type: 'String' }],
                                    numberProperty: 7,
                                    emptyArrayItems: { type: 'array', items: [] },
                                    falseItems: { type: 'array', items: false },
                                    itemsAndProperties: {
                                        type: 'object',
                                        properties: { p: { $ref: '#/components/schemas/Leaf' } },
                                        items: { $ref: '#/components/schemas/Leaf' }
                                    },
                                    nullType: { type: null, description: 'n' },
                                    badType: { type: 'bogus', description: 'b' },
                                    ctorType: { type: 'function Boolean() { [native code] }' },
                                    listType: { type: ['Integer', 'bogus'] },
                                    nestedCtor: { type: 'function Object() {}', properties: { q: { type: 'BOOLEAN' } } },
                                    itemsCtor: { type: 'function Array() {}', items: { type: 'function Date() {}' } },
                                    emptyProperties: { type: 'object', properties: {} },
                                    '10': { type: 'string' },
                                    '2': { type: 'string' }
                                }
                            }
                        }
                    }
                }
            })
        }
    },
    components: {
        schemas: {
            Leaf: { type: 'object', properties: { x: { type: 'integer' } } },
            A: { type: 'object', properties: { b: { $ref: '#/components/schemas/B' } } },
            B: { type: 'object', properties: { c: { $ref: '#/components/schemas/C' } } },
            C: { type: 'object', properties: { leaf: { $ref: '#/components/schemas/Leaf' } } },
            Self: { type: 'object', properties: { me: { $ref: '#/components/schemas/Self' }, x: { type: 'string' } } },
            Ping: { type: 'object', properties: { pong: { $ref: '#/components/schemas/Pong' } } },
            Pong: { type: 'object', properties: { ping: { $ref: '#/components/schemas/Ping' } } },
            AString: 'xyz',
            ANumber: 5,
            AnArray: ['p', { type: 'string' }],
            ATrue: true,
            ANull: null,
            'A~1B': { type: 'string', description: 'escaped' },
            'A/B': { type: 'string', description: 'slash' },
            'With Space': { type: 'string', description: 'space' }
        }
    }
};

const names = {
    openapi: '3.0.0',
    info,
    paths: {
        '/': { get: op({}) },
        '/a//b': { get: op({}) },
        '/trailing/': { get: op({}) },
        '/x/{a}/y/{b}': { delete: op({}) },
        '/under__scores/{id}': { patch: op({}) },
        '/dots.and-dashes': { put: op({}) },
        '/empty-operation-id': { get: op({ operationId: '' }) },
        '/described': { get: op({ operationId: 'described', description: 'Says something' }) },
        '/very/long/path/that/goes/on/and/on/for/many/segments/beyond/the/limit': { post: op({}) },
        '/long': { get: op({ operationId: 'anOperationIdThatIsLongEnoughToPushTheToolNameBeyondSixtyFourCharacters' }) },
        '/duplicate/one': { get: op({ operationId: 'dup' }) },
        '/duplicate/two': { get: op({ operationId: 'dup' }) },
        '/methods': {
            options: op({}),
            head: op({ operationId: 'headOp' }),
            search: op({})
        }
    }
};

const documentCases = [
    { name: 'users fixture', services: { svc: fixture('users-service.json') } },
    { name: 'two services', services: { svc: fixture('users-service.json'), other: fixture('default-service.json') } },
    { name: 'types', services: { t: types } },
    { name: 'bodies', services: { b: bodies } },
    { name: 'refs', services: { r: refs } },
    { name: 'names', services: { n: names } }
];

const registryCases = documentCases.map(({ name, services }) => ({
    name,
    services,
    reference: registry(structuredClone(services), false),
    d8: registry(structuredClone(services), true)
}));

// ---------------------------------------------------------------------------------------------
// Tool calls

function requestOptions(tool, args, middlewareHeaders, excludeHeaders) {
    const executor = new McpToolExecutorService(null, null, null, null, { apiServices: [], excludeHeaders });
    const proxyRequest = new ProxyRequest();
    for (const [name, value] of middlewareHeaders) {
        proxyRequest.addHeader(name, value);
    }
    const options = executor.buildRequestOptions(tool, args, proxyRequest);
    return {
        method: options.method,
        path: options.path,
        headers: [...options.headers.entries()],
        body: options.body ?? null
    };
}

const userTool = { httpMethod: 'GET', path: '/users/{id}' };
const callCases = [
    { name: 'path, query, header', tool: userTool, args: { path_id: '42', query_status: 'active', header_locale: 'en' } },
    { name: 'no arguments', tool: { httpMethod: 'GET', path: '/health' }, args: {} },
    { name: 'trailing slash kept', tool: { httpMethod: 'GET', path: '/health/' }, args: {} },
    { name: 'unknown shapes', tool: userTool, args: { path_id: '1', other: 'x', Path_x: 1, bodyx: 2, header: 'h', query: 'q' } },
    { name: 'missing path argument', tool: userTool, args: { path_nope: 'x' } },
    { name: 'repeated placeholder', tool: { httpMethod: 'GET', path: '/a/{id}/b/{id}' }, args: { path_id: '7' } },
    { name: 'two placeholders', tool: { httpMethod: 'GET', path: '/a/{x}/b/{y}' }, args: { path_y: 'Y', path_x: 'X' } },
    { name: 'path value types', tool: { httpMethod: 'GET', path: '/{a}/{b}/{c}/{d}/{e}' }, args: { path_a: 12.0, path_b: true, path_c: null, path_d: 1e21, path_e: -0 } },
    {
        name: 'query stringification',
        tool: userTool,
        args: {
            path_id: '1',
            query_s: 'a b&c=d/é?+*-._~!()\'%#[]',
            query_n: 1.5,
            query_int: 10,
            query_big: 1e21,
            query_small: 1e-7,
            query_tiny: 0.000001,
            query_neg: -2.5,
            query_neg0: -0,
            query_huge: 12345678901234567890,
            query_t: true,
            query_f: false,
            query_null: null,
            query_obj: { a: 1 },
            query_arr: [1, [2, 3], null, { x: 1 }, 'z', true],
            query_emptyArr: [],
            query_nestedEmpty: [[], [null]],
            query_empty: '',
            query_: 'blank name',
            'query_a b': 'space in name',
            query_10: 'ten',
            query_2: 'two',
            'query_é': 'accented name',
            query_emoji: '\u{1F600}',
            query_ctrl: '\u0001\n\t'
        }
    },
    { name: 'empty query value', tool: userTool, args: { path_id: '1', query_a: '' } },
    { name: 'duplicate query name', tool: userTool, args: { path_id: '1', query_a: 'first', 'query_a ': 'spaced' } },
    {
        name: 'header precedence',
        tool: userTool,
        args: {
            path_id: '7',
            'header_x-request-id': 'arg',
            'header_Content-Type': 'text/plain',
            'header_content-type': 'text/x',
            'header_x-empty': '',
            'header_Auth-User-Id': 'spoof',
            'header_permission-in-any-guard': 'spoof',
            'header_x-num': 0,
            'header_x-false': false,
            'header_x-null': null,
            header_7: 'seven',
            'header_X-Up': 'Up',
            'header_x-obj': { a: 1 }
        },
        middleware: [
            ['x-request-id', 'mw'],
            ['xEmpty', ''],
            ['x-mw', '1'],
            ['X-Up', 'mw'],
            ['x-num', ''],
            ['authUserId', '99']
        ],
        excludeHeaders: ['auth-user-id', 'permission-in-any-guard']
    },
    {
        name: 'middleware only',
        tool: userTool,
        args: { path_id: '7' },
        middleware: [['authUserId', '1'], ['xTraceId', 'abc'], ['3', 'three']],
        excludeHeaders: ['auth-user-id']
    },
    {
        name: 'excluded header argument, no middleware',
        tool: userTool,
        args: { path_id: '42', 'header_Auth-User-Id': 'spoofed' },
        excludeHeaders: ['auth-user-id']
    },
    { name: 'content-type from middleware', tool: userTool, args: { path_id: '1' }, middleware: [['contentType', 'text/csv']] },
    { name: 'POST object body', tool: { httpMethod: 'POST', path: '/users' }, args: { body: { b: 1.0, a: [1, 'x', null], c: 'é', d: 1e21, '2': 'two', '1': 'one', e: { '10': 1, z: 2, '3': 3 } } } },
    { name: 'POST string body', tool: { httpMethod: 'POST', path: '/users' }, args: { body: 'str' } },
    { name: 'POST number body', tool: { httpMethod: 'POST', path: '/users' }, args: { body: 12.50 } },
    { name: 'POST true body', tool: { httpMethod: 'POST', path: '/users' }, args: { body: true } },
    { name: 'POST empty object body', tool: { httpMethod: 'POST', path: '/users' }, args: { body: {} } },
    { name: 'POST empty array body', tool: { httpMethod: 'POST', path: '/users' }, args: { body: [] } },
    { name: 'POST 0 body', tool: { httpMethod: 'POST', path: '/users' }, args: { body: 0 } },
    { name: 'POST false body', tool: { httpMethod: 'POST', path: '/users' }, args: { body: false } },
    { name: 'POST null body', tool: { httpMethod: 'POST', path: '/users' }, args: { body: null } },
    { name: 'POST empty string body', tool: { httpMethod: 'POST', path: '/users' }, args: { body: '' } },
    { name: 'POST control characters', tool: { httpMethod: 'POST', path: '/users' }, args: { body: { s: '\u0000\u001f\b\f\n\r\t"\\/ \u007f' } } },
    { name: 'PUT body', tool: { httpMethod: 'PUT', path: '/users/{id}' }, args: { path_id: '1', body: { a: 1 } } },
    { name: 'PATCH body', tool: { httpMethod: 'PATCH', path: '/users/{id}' }, args: { path_id: '1', body: [1] } },
    { name: 'GET body ignored', tool: userTool, args: { path_id: '1', body: { a: 1 } } },
    { name: 'DELETE body ignored', tool: { httpMethod: 'DELETE', path: '/users/{id}' }, args: { path_id: '1', body: { a: 1 } } },
    { name: 'HEAD', tool: { httpMethod: 'HEAD', path: '/users/{id}' }, args: { path_id: '1' } },
    { name: 'OPTIONS body ignored', tool: { httpMethod: 'OPTIONS', path: '/users' }, args: { body: { a: 1 } } },
    { name: 'SEARCH body ignored', tool: { httpMethod: 'SEARCH', path: '/users' }, args: { body: { a: 1 } } },
    { name: 'later body argument wins', tool: { httpMethod: 'POST', path: '/users' }, args: { body: { a: 1 }, query_x: 1 } }
];

// Cases whose path arguments Deviation D7 changes: the reference substitutes them raw.
const d7Cases = [
    { name: 'traversal', args: { path_id: '../admin' } },
    { name: 'slash', args: { path_id: 'a/b' } },
    { name: 'dot', args: { path_id: '.' } },
    { name: 'dot dot', args: { path_id: '..' } },
    { name: 'empty', args: { path_id: '' } },
    { name: 'query injection', args: { path_id: 'x?admin=1' } },
    { name: 'fragment', args: { path_id: 'x#y' } },
    { name: 'replacement pattern', args: { path_id: 'x$&y' } },
    { name: 'placeholder in value', args: { path_id: '{id}' } },
    { name: 'percent', args: { path_id: '%2e%2e' } },
    { name: 'space and unicode', args: { path_id: 'a bé' } }
].map((c) => ({ ...c, tool: userTool }));

const call = callCases.map((c) => ({
    name: c.name,
    tool: c.tool,
    args: c.args,
    middleware: c.middleware ?? [],
    excludeHeaders: c.excludeHeaders ?? [],
    reference: requestOptions(c.tool, structuredClone(c.args), c.middleware ?? [], c.excludeHeaders ?? [])
}));
const d7 = d7Cases.map((c) => ({
    name: c.name,
    tool: c.tool,
    args: c.args,
    reference: requestOptions(c.tool, structuredClone(c.args), [], [])
}));

const write = (file, value) => writeFileSync(join(here, file), JSON.stringify(value, null, 1) + '\n');
write('registry.json', { versions, cases: registryCases });
write('call.json', { versions, cases: call, d7 });
console.log(`registry.json: ${registryCases.length} cases, call.json: ${call.length} + ${d7.length} cases`);
