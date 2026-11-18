// Generates `path-to-regexp.json`: golden vectors for the Rust port of `path-to-regexp` 8
// `match()` in `crates/rust-gateway/src/route/pattern.rs`.
//
// It runs the copy of `path-to-regexp` that the Reference Gateway itself resolves, from the
// conformance reference app, so the vectors describe the reference's behaviour exactly.
//
// Usage, from the repository root, after `pnpm install --frozen-lockfile` in
// `conformance/reference`:
//
//     node crates/rust-gateway/src/route/testdata/generate-path-to-regexp.mjs
//
// Commit the output together with any change to this script.

import { createRequire } from 'node:module';
import { realpathSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '../../../../..');
const gateway = realpathSync(join(root, 'conformance/reference/node_modules/@hodfords/nestjs-api-gateway'));
const require = createRequire(join(gateway, 'package.json'));
const ptrPath = require.resolve('path-to-regexp');
const { match, pathToRegexp } = require('path-to-regexp');
const version = JSON.parse(
    (await import('node:fs')).readFileSync(join(dirname(ptrPath), '..', 'package.json'), 'utf8')
).version;

// Inputs tried against every pattern.
const common = [
    '',
    '/',
    '//',
    '/a',
    '/A',
    '/a/',
    '/a//',
    '/a/b',
    '/a/b/',
    '/a/b/c',
    '/b',
    '/a-b',
    '/a.b',
    '/a.b.c',
    '/users',
    '/USERS',
    '/users/',
    '/users//',
    '/users/42',
    '/users/42/',
    '/users/42/extra',
    '/users/me',
    '/users/a%2Fb',
    '/users/caf%C3%A9',
    '/users/a;b',
    '/users/%zz',
    '/users/%C3',
    '/users/%E0%A4%A',
    '/users/%F0%9F%98%80',
    '/users/%ED%A0%80',
    '/users/%C0%AF',
    '/users/%41%42',
    '/files',
    '/files/',
    '/files/a',
    '/files/a/b/c',
    '/files/a/b/c/',
    '/files/a%2Fb/c',
    '/files/%zz/c',
    '/test',
    '/tests',
    '/x/y.json',
    'users',
    'a/b',
];

// Patterns, each with inputs specific to it (tried in addition to `common`).
const patterns = [
    ['', []],
    ['/', []],
    ['/a', ['/a/', '/a//']],
    ['/users', []],
    ['/users/:id', ['/users/:id', '/Users/42']],
    ['/users/:id/posts/:postId', ['/users/1/posts/2', '/users/1/posts/2/', '/users/1/posts/', '/users//posts/2']],
    ['/:a-:b', ['/a-b', '/a-b-c', '/-b', '/a-', '/a--b', '/x-y/']],
    ['/:a.:b', ['/a.b', '/a.b.c', '/.b', '/a.']],
    ['/:a-:b-:c', ['/a-b-c', '/a-b-c-d', '/a--c', '/a-b']],
    ['/:name.:ext', ['/file.tar.gz', '/file.', '/.ext']],
    ['/:id.json', ['/42.json', '/42.JSON', '/.json', '/a.json.json']],
    ['/:a:b', []],
    ['/:a*b', []],
    ['/files/*path', ['/files/a/b/c', '/files/a/b/c/', '/files//', '/files//a']],
    ['/files/*path/edit', ['/files/a/edit', '/files/a/b/edit', '/files/edit', '/files/a/edit/']],
    ['/*a/*b', ['/x/y', '/x/y/z', '/x']],
    ['/:a/*b', ['/x/y', '/x/y/z', '/x']],
    ['/*path.json', ['/a.json', '/a/b.json', '/.json', '/a.json/b.json']],
    ['/*path-:id', ['/a-1', '/a/b-1', '/a-b-1']],
    ['/:id/*rest', ['/1/2/3', '/1']],
    ['/x*y', ['/x', '/xa', '/xa/b', '/x/']],
    ['*all', ['/anything/at/all']],
    ['*', []],
    [':', []],
    ['/:', []],
    ['/v1/paths/optional{/:id}', ['/v1/paths/optional', '/v1/paths/optional/5', '/v1/paths/optional/5/6', '/v1/paths/optional/']],
    ['/v1/paths/escaped\\:literal', ['/v1/paths/escaped:literal', '/v1/paths/escaped%3Aliteral', '/v1/paths/escaped']],
    ['/v1/paths/files/*path', ['/v1/paths/files/a', '/v1/paths/files/a/b/c', '/v1/paths/files/', '/v1/paths/files']],
    ['{/:a}{/:b}', ['/1', '/1/2', '/1/2/3']],
    ['/a{/b{/c}}', ['/a', '/a/b', '/a/b/c', '/a/c']],
    ['/test{s}', ['/test', '/tests', '/testss']],
    ['/:a{-:b}', ['/x', '/x-y', '/x-y-z', '/x-']],
    ['/users/{a-b}', ['/users/', '/users/a-b', '/users']],
    ['/:"quoted name"', ['/x']],
    ['/:"a\\"b"', ['/x']],
    ['/:"a/b"/x', ['/1/x']],
    ['/:"unterminated', []],
    ['/:"', []],
    ['/:""', []],
    ['/:"abc\\', []],
    ['/:"1"/:"2"', ['/a/b']],
    ['/(', []],
    ['/)', []],
    ['/[', []],
    ['/]', []],
    ['/+', []],
    ['/?', []],
    ['/!', []],
    ['/{', []],
    ['/}', []],
    ['/{a', []],
    ['/a}', []],
    ['/\\', []],
    ['/a\\', []],
    ['/\\(x\\)', ['/(x)', '/x']],
    ['/v1/items(:id)', []],
    ['/v1/items:batch', ['/v1/itemsX', '/v1/items:batch', '/v1/items']],
    ['/v1/items\\:batch', ['/v1/items:batch', '/v1/itemsX']],
    ['/:id/:id', ['/1/2']],
    ['/:café', ['/x']],
    ['/:ab$c', ['/x']],
    ['/:_x', ['/x']],
    ['/:$', ['/x']],
    ['/:1a', []],
    ['/:a‌b', ['/x']],
    ["/user's", ["/user's", '/USER\'S']],
    ['/a|b', ['/a|b', '/a', '/b']],
    ['/a^b$c', ['/a^b$c']],
    ['/a.b', ['/aXb', '/a.b']],
    ['/café/:id', ['/café/1', '/CAFÉ/1', '/caf%C3%A9/1']],
    ['/ſ', ['/s', '/S', '/ſ']],
    ['/K', ['/k', '/K', '/K']],
    ['/k', ['/K', '/K']],
    ['/s', ['/ſ', '/S']],
    ['/straße', ['/STRASSE', '/straße', '/STRAßE']],
    ['/é', ['/É', '/é']],
    ['/:id/\u{1f600}', ['/1/\u{1f600}', '/\u{1f600}/\u{1f600}']],
    ['/\u{1f600}/:id', ['/\u{1f600}/1']],
    ['/:a-\u{1f600}', ['/x-\u{1f600}', '/x-\u{1f600}-\u{1f600}']],
    ['/a\nb', ['/a\nb', '/a\nB']],
    ['/:aé:b', ['/xéy']],
    ['/:aa-:b', ['/x-y']],
    ['/:a--:b', ['/x--y', '/x-y', '/x---y']],
    ['/:a..:b', ['/x..y', '/x...y']],
    ['/:a-b-:c', ['/x-b-y', '/x-b-b-y']],
    ['/*a-b-*c', ['/x-b-y', '/x/y-b-z/w']],
    ['/*a/x/*b', ['/1/x/2', '/1/x/x/2', '/x/x/x']],
    ['/:a/x/*b', ['/1/x/2']],
    ['/*a.*b', ['/x.y', '/x.y.z', '/x/y.z']],
    ['/:a.*b', ['/x.y', '/x.y/z']],
    ['/*a.:b', ['/x.y', '/x/y.z', '/x.y.z']],
    ['/files/*path/:name.:ext', ['/files/a/b/c.txt', '/files/c.txt']],
    ['/{:a}', ['/', '/x']],
    ['{/*rest}', ['/', '', '/a/b']],
    ['/a{b}c', ['/ac', '/abc']],
    ['{a}{b}{c}{d}{e}{f}{g}{h}', ['abcdefgh', '', 'aceg']],
    ['{a}{b}{c}{d}{e}{f}{g}{h}{i}', []],
    ['{:a}{:b}', []],
    ['{x:a}{:b}', []],
    ['/:a{/x/:b}', ['/1', '/1/x/2']],
    [['/a', '/b/:id'], ['/b/1']],
    [[], []],
    [['/a', ['/b', '/c/:x']], ['/c/1']],
    [['/:x:y'], []],
    [['/ok', '/(bad'], []],
    [['/:a', '/:a/:b'], ['/1/2']],
];

function run(pattern, input) {
    const fn = match(pattern, { decode: decodeURIComponent });
    try {
        const result = fn(input);
        if (result === false) return false;
        return { path: result.path, params: { ...result.params } };
    } catch (error) {
        return { error: `${error.name}: ${error.message}` };
    }
}

const LINE_TERMINATORS = ['\n', '\r', String.fromCharCode(0x2028), String.fromCharCode(0x2029)];
const hasLineTerminator = (value) =>
    [value].flat(Infinity).some((s) => LINE_TERMINATORS.some((t) => s.includes(t)));

const out = [];
for (const [pattern, extra] of patterns) {
    let entry = { pattern };
    try {
        const { regexp, keys } = pathToRegexp(pattern);
        entry.source = hasLineTerminator(pattern) ? null : regexp.source;
        entry.keys = keys.map((k) => ({ name: k.name, type: k.type }));
        const inputs = [...new Set([...common, ...extra])];
        entry.cases = inputs.map((input) => ({ input, result: run(pattern, input) }));
    } catch (error) {
        entry.error = `${error.name}: ${error.message}`;
    }
    out.push(entry);
}

function ranges(test) {
    const result = [];
    let start = -1;
    for (let cp = 0; cp <= 0x10ffff; cp++) {
        const ok = !(cp >= 0xd800 && cp <= 0xdfff) && test(String.fromCodePoint(cp));
        if (ok && start < 0) start = cp;
        if (!ok && start >= 0) {
            result.push([start, cp - 1]);
            start = -1;
        }
    }
    if (start >= 0) result.push([start, 0x10ffff]);
    return result;
}

const ID_START = /^[$_\p{ID_Start}]$/u;
const ID_CONTINUE = /^[$‌‍\p{ID_Continue}]$/u;

const vectors = {
    generator: 'crates/rust-gateway/src/route/testdata/generate-path-to-regexp.mjs',
    pathToRegexpVersion: version,
    node: process.version,
    unicode: process.versions.unicode,
    options: { decode: 'decodeURIComponent' },
    patterns: out,
    idStart: ranges((c) => ID_START.test(c)),
    idContinue: ranges((c) => ID_CONTINUE.test(c)),
};

writeFileSync(join(here, 'path-to-regexp.json'), JSON.stringify(vectors, null, 1) + '\n');

// The identifier tables the Rust tokenizer uses, so it follows Node's Unicode version.
const table = (name, doc, list) =>
    `/// ${doc}\npub(super) const ${name}: &[(u32, u32)] = &[\n` +
    list.map(([lo, hi]) => `    (0x${lo.toString(16).toUpperCase()}, 0x${hi.toString(16).toUpperCase()}),\n`).join('') +
    '];\n';
writeFileSync(
    join(here, '..', 'pattern', 'id_tables.rs'),
    `//! Generated by \`testdata/generate-path-to-regexp.mjs\` from Node ${process.version} (Unicode ${process.versions.unicode}). Do not edit.\n\n` +
        table('ID_START', 'Code point ranges matching `/^[$_\\p{ID_Start}]$/u`.', vectors.idStart) +
        '\n' +
        table('ID_CONTINUE', 'Code point ranges matching `/^[$\\u200c\\u200d\\p{ID_Continue}]$/u`.', vectors.idContinue)
);
console.log(`wrote ${out.length} patterns with path-to-regexp ${version}`);
