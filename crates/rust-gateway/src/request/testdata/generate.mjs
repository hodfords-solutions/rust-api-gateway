// Golden vectors for `api-gateway`'s request module, produced by the code the Reference
// Gateway actually runs: Node's WHATWG `URL`, `node:querystring`, Express 5's router and
// `parseurl`, es-toolkit `kebabCase`, and Node's HTTP parser.
//
// The libraries are loaded from `conformance/reference/node_modules`, so the vectors match the
// versions pinned there. Run from the repository root after installing the reference app:
//
//     (cd conformance/reference && pnpm install --frozen-lockfile)
//     node crates/rust-gateway/src/request/testdata/generate.mjs
//
// Outputs (commit them):
//   testdata/url.json         REQ-02  target -> pathname + search, or null when `new URL` throws
//   testdata/path_param.json  REQ-26  target -> the segment Express fails to decode, or null
//   testdata/query.json       FWD-04  raw query -> normalised query -> what undici sends
//   testdata/kebab.json       REQ-21  key -> es-toolkit kebabCase(key)
//   testdata/headers.json     FWD-12  raw header lines -> Node's `req.headers` entries
//   ../unicode_tables.rs      Unicode property ranges for the kebab-case port

import { createRequire } from 'node:module';
import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import http from 'node:http';
import net from 'node:net';
import querystring from 'node:querystring';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '../../../../..');
const reference = createRequire(join(root, 'conformance/reference/package.json'));
const gateway = createRequire(reference.resolve('@hodfords/nestjs-api-gateway'));
const express = createRequire(reference.resolve('express'));
const routerRequire = createRequire(express.resolve('router'));

const { kebabCase } = gateway('es-toolkit');
const parseurl = routerRequire('parseurl');
const Layer = routerRequire('./lib/layer.js');

const versions = {
    node: process.version,
    'es-toolkit': gateway('es-toolkit/package.json').version,
    undici: gateway('undici/package.json').version,
    router: express('router/package.json').version,
    express: reference('express/package.json').version,
};

// Deterministic pseudo-random choices (mulberry32), so reruns produce the same files.
function rng(seed) {
    let a = seed >>> 0;
    return () => {
        a = (a + 0x6d2b79f5) >>> 0;
        let t = a;
        t = Math.imul(t ^ (t >>> 15), t | 1);
        t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
}
const pick = (r, list) => list[Math.floor(r() * list.length)];
const unique = (list) => [...new Set(list)];

function write(name, cases) {
    const body = { generatedBy: 'generate.mjs', versions, cases };
    writeFileSync(join(here, name), JSON.stringify(body, null, 1) + '\n');
    console.log(`${name}: ${cases.length} cases`);
}

// ---------------------------------------------------------------------------------------------
// REQ-02: `new URL(target, 'http://dummy-base.local')` then `pathname + search`.

const base = 'http://dummy-base.local';
function normalise(target) {
    try {
        const url = new URL(target, base);
        return url.pathname + url.search;
    } catch {
        return null;
    }
}

const segmentAtoms = [
    '', '.', '..', '%2e', '%2E', '.%2e', '%2e.', '%2e%2E', '...', 'a', 'B', 'svc', 'v1', '%41', '%zz',
    '%', '%4', '%e9', '%C3%A9', '..%2f', '%2f', '%2F', '%5C', ';', ';a=b', '~', '^', '|', '"', '<', '>',
    '`', '{', '}', "'", '[', ']', '@', ':', '!', '$', '&', '(', ')', '*', '+', ',', '=', 'é', '😀', 'a b',
    '\\', 'x\\..', '\t', '\n', '\r', '\u0001', '\u007f', '%00', '#', '?', 'a.', '.a', '%2e%2e%2f',
];
const prefixes = ['/', '//', '\\', '/\\', '\\\\', '///', 'http://h', 'http://h:80', 'HTTP://H', 'https://h',
    'ws://h', 'file:///', 'http:', 'http:/', 'mailto:', 'javascript:', 'a:', ' /', '\t/', '//h', ''];
const suffixes = ['', '?', '?a=1', "?a b", "?'", '?"<>`', '?#', '#f', '?a#b', '?%zz', '??', '?a=1&b',
    '?é', '?{}|^', '?\\', '?a\tb', '#', '#?x'];

function urlCases() {
    const fixed = [
        '/user-services/v1/me?x=1&y=2', '/user-services/oidc/../v1/admin', '/user-services/oidc/%2e%2e/v1/admin',
        '/user-services/oidc/.%2e/v1/admin', '/user-services/oidc\\..\\v1/admin',
        '/user-services/oidc/./a/../../v1/admin?k=v', '/../user-services/v1/admin', '', '/', '*', '//[bad/x',
        '//[::1]/x', '//[::1/x', '//a b/x', '//%zz/x', '//h:99999/x', '//h:x/x', '//h@/x', '//@h/x',
        '//user:pass@h/x', '//h/x?y', 'http://other.example/svc/v1/users?x=1', 'http://other.example',
        'http://other.example?x', '/svc/v1/users#frag', '/svc/v1/users?a=1#frag', '/svc/v1//users',
        '/svc/v1/..%2fusers', '/svc/v1/users;x=1', '/svc/%7Ev1', '/svc/a"b<c>d`e{f}', "/svc/a^b|c'd",
        "/svc/v1/users?q='x'", '//svc/v1/users', '//host', '/svc//v1/users', '/C:/x', '/c|/x', '/./.',
        '/..', '/%2e', '/a/b/c/../../..', '/a/b/c/../../../..', '/a/%2e%2E/b', ' \t/a\n/b\r ', '/a\u0000b',
        'http://h/a/../../b', 'http://h/%2e%2e/b', 'blob:http://h/x', 'data:,x', 'about:blank', 'h:/x',
        '/a?b?c', '/a?%', '/a?%2', '/a?\u0000', '/%', '/a%2', '/\u00a0', '/\ufeff', '\ufeff/x',
    ];
    const r = rng(2);
    const generated = [];
    for (let i = 0; i < 4000; i++) {
        const n = 1 + Math.floor(r() * 4);
        const segments = Array.from({ length: n }, () => pick(r, segmentAtoms));
        generated.push(pick(r, prefixes) + segments.join(r() < 0.15 ? '\\' : '/') + pick(r, suffixes));
    }
    return unique([...fixed, ...generated]).map((target) => [target, normalise(target)]);
}

// ---------------------------------------------------------------------------------------------
// REQ-26: Express's router matches `/{*splat}` against `parseurl(req).pathname` and decodes each
// segment of the wildcard with `decodeURIComponent`; the first failure answers 400.

const splat = new Layer('/{*splat}', { end: true, strict: false, sensitive: false }, () => {});
function malformedParam(target) {
    let pathname;
    try {
        pathname = parseurl({ url: target }).pathname;
    } catch {
        return { error: 'parseurl' };
    }
    try {
        splat.match(pathname);
        return null;
    } catch (error) {
        const match = /^Failed to decode param '(.*)'$/s.exec(error.message);
        if (!match) throw error;
        return match[1];
    }
}

const pathAtoms = ['a', 'svc', '%41', '%zz', '%', '%4', '%e9', '%C3%A9', '%C3', '%F0%9F%98', '%F0%9F%98%80',
    '%ED%A0%80', '%C0%AF', '%2F', '%2f', '%25', '%%', '%G0', '%0G', '..', '.', '', '\\', "a'b", 'a"b', 'é',
    '{', '|', '^', '`', '<', '>', ';', '+', '%20', '~'];
const pathPrefixes = ['/', '//', 'http://h/', 'http://h', 'HTTP://H/', '/\\', '*', 'h', '\\'];
const pathSuffixes = ['', '/', '?q=%zz', '?q', '#%zz', '#x', '?a#b', '/#', '?%zz#%zz'];

function pathParamCases() {
    const fixed = ['/svc/v1/users/%zz', '/svc/%zz/users', '/%zz/svc/v1/users', '/svc/a%zz/%yy',
        '/svc/v1/users/%zz%41', '/svc/v1/users/%41%zz', '/svc/a%2Fb%zz', '/svc/v1/users/%', '/svc/v1/users/%4',
        '/svc/v1/users/%e9', '/svc/v1/users/%F0%9F%98', '/svc/v1/users/%ED%A0%80', '/svc/v1/users/%C0%AF',
        '/svc/v1/users/%C3%A9%', '/svc/v1/users/%zz/', '//%zz', "/svc/a'b\"c%zz", '/svc\\%zz',
        '/svc/%zz/../v1/users', '/svc/v1/users?q=%zz', '/svc/v1/users#%zz', '/svc/v1/users/%zz#x',
        'http://other.example/svc/%zz', '/', '*', 'http://h', 'http://h?x', '/a"b%zz#x', '/a\\b%zz#x',
        '/a^b%zz#', 'http://h/a"%zz', 'http://h\\a%zz', 'http://h\\a/%zz', '/%zz?#', '/?%zz'];
    const r = rng(3);
    const generated = [];
    for (let i = 0; i < 3000; i++) {
        const n = 1 + Math.floor(r() * 3);
        const segments = Array.from({ length: n }, () => pick(r, pathAtoms) + (r() < 0.3 ? pick(r, pathAtoms) : ''));
        generated.push(pick(r, pathPrefixes) + segments.join('/') + pick(r, pathSuffixes));
    }
    return unique([...fixed, ...generated]).map((target) => [target, malformedParam(target)]);
}

// ---------------------------------------------------------------------------------------------
// FWD-04: Express 5 parses `req.query` with `querystring.parse` (from the normalised URL) and
// undici serialises it with `querystring.stringify`.

function forwardedQuery(raw) {
    const normalised = normalise('/p?' + raw);
    if (normalised === null) return null;
    const search = normalised.slice(2); // drop "/p"
    const query = search.startsWith('?') ? search.slice(1) : '';
    return [query, querystring.stringify(querystring.parse(query))];
}

const keyAtoms = ['a', 'b', 'A', '', '%41', '%zz', '%', '%4', '+', '%2B', '%20', 'a+b', '[', ']', 'a[b]',
    'a[]', '__proto__', 'constructor', 'toString', 'hasOwnProperty', 'length', '0', '1', '2', '10', '01', '-1',
    '4294967294', '4294967295', '1.5', '😀', '%F0%9F%98%80', '%e9', '%C3%A9', '%C3', "'", '"', '~!*()', '\\',
    'é', 'a.b', ' ', '%00', '%7f', '%2526', 'x%41%zz', '%%41', '%4%41', '%c3%a9%e9'];
const valueAtoms = [...keyAtoms, '1=2', '==', 'a=b=c', '{x}|`y`^', '<x>', ';', 'a;b'];

function queryCases() {
    const fixed = ['a=1', 'a=1&a=2', 'a=1&b=2&a=3', 'q=a%20b', 'q=caf%C3%A9', 'q=%2F%3F%26%3D%23', 'a=', 'a',
        'a&b=', '', 'q=a+b', 'q=%2B', 'q=%zz', 'q=%', 'q=%41', 'a[b]=1', 'a[]=1&a[]=2', 'a%5Bb%5D=1', 'a.b=1',
        '=v', '&&a=1&&', 'a=1=2', 'a=1;b=2', 'A=1&a=2', "q=it's", 'q=~!*()', 'q="x"', 'q=<x>', 'q={x}|`y`^',
        'q=a\\b', '__proto__=x', 'constructor=x', 'b=1&2=x&1=y&a=0', '=', '==', '&', '&=&', '=&=', '+=+',
        '%=%', 'a=%41%zz', 'a%41%zz=1', 'q=%e9', 'q=%C3', 'q=%C3%28', 'q=%F0%9F%98', 'q=%ED%A0%80',
        'a=1&a=2&a=3&b=1&a=4', 'k=v&k&k=', 'x=%2526', 'q=%%41', 'q=%4%41', 'a#b=1', 'a=b#c', 'a=\t1', 'é=è'];
    const many = Array.from({ length: 1005 }, (_, i) => `k${i}=${i}`).join('&');
    const manyEmpty = '&'.repeat(999) + 'a=1';
    const r = rng(4);
    const generated = [];
    for (let i = 0; i < 4000; i++) {
        const n = 1 + Math.floor(r() * 4);
        const pairs = Array.from({ length: n }, () => {
            const form = r();
            const key = pick(r, keyAtoms) + (r() < 0.3 ? pick(r, keyAtoms) : '');
            const value = pick(r, valueAtoms) + (r() < 0.3 ? pick(r, valueAtoms) : '');
            if (form < 0.15) return key;
            if (form < 0.25) return key + '=';
            if (form < 0.3) return '=' + value;
            return key + '=' + value;
        });
        generated.push(pairs.join(r() < 0.1 ? '&&' : '&'));
    }
    return unique([...fixed, many, manyEmpty, ...generated]).map((raw) => {
        const result = forwardedQuery(raw);
        return result === null ? [raw, null, null] : [raw, result[0], result[1]];
    });
}

// ---------------------------------------------------------------------------------------------
// REQ-21: es-toolkit kebabCase.

function kebabCases() {
    const fixed = ['authUserId', 'xRequestId', 'auth_user_id', 'auth-user-id', 'someKey', 'oauth2Token',
        'HTTPRequest', 'XMLHttpRequest', 'camelCaseHTTPRequest🚀', 'a1b2', 'A1B', 'x__y', '-leading', 'trailing-',
        '', '123', 'a-1', 'ABCdef', 'aBC', 'ABC', 'ÀÉÎ', 'straße', 'ǅungla', '日本語', '日本Abc', '😀', 'a😀b',
        'Ⅷx', 'ªb', 'ᴬb', 'x\u0301y', 'Lunedì 18 Set', 'fred, barney, & pebbles', 'some whitespace',
        'hyphen-text', 'x-test-route', 'X-Test-Route', 'permissionInAnyGuard', 'host', 'HOST', 'contentType',
        'iOS', 'IOSVersion', 'v2Api', 'API_KEY', 'apiKey2FA', '2FA', '__proto__', 'a.b', 'a/b', 'a b', '\t',
        'Ⓐbc', 'ǈ', 'ﬀ', 'İstanbul', 'ΣΑΣ', 'σας', 'ß', '\u2028', '#', '1️⃣', '©x', '™x', '↔x', '⌚x', '🇻🇳'];
    const pool = [...'abcxyzABCXYZ0129', '-', '_', ' ', '.', '/', '@', '$', '😀', 'é', 'É', 'ß', 'Σ', '日', 'ǅ',
        'ª', 'ᴬ', '©', '\u0301', '1', 'Ⅷ'];
    const r = rng(5);
    const generated = [];
    for (let i = 0; i < 3000; i++) {
        const n = 1 + Math.floor(r() * 10);
        generated.push(Array.from({ length: n }, () => pick(r, pool)).join(''));
    }
    return unique([...fixed, ...generated]).map((key) => [key, kebabCase(key)]);
}

// ---------------------------------------------------------------------------------------------
// FWD-12: how Node's HTTP parser turns raw header lines into `req.headers` (lower-cased names,
// object key order, duplicates dropped, joined with ", " or "; ", or kept as an array).

const headerCases = [
    [['X-Rep', '1'], ['X-Rep', '2']],
    [['Cookie', 'a=1'], ['Cookie', 'b=2']],
    [['Set-Cookie', 's=1'], ['Set-Cookie', 's=2']],
    [['42', 'n'], ['A', '1'], ['7', 'm'], ['0', 'z'], ['01', 'o']],
    [['X-Empty', ''], ['X-Empty', 'v']],
    [['X-Empty', ''], ['X-Empty', '']],
    [['Accept', 'a'], ['ACCEPT', 'b'], ['accept', 'c']],
    [['X-Spaces', '  padded  ']],
    [['Date', 'd1'], ['Date', 'd2']],
    [['Vary', 'a'], ['Vary', 'b']],
    [['Connection', 'keep-alive'], ['Connection', 'x-a']],
    [['Upgrade', 'a'], ['Upgrade', 'b']],
    [['X-Forwarded-For', '1.1.1.1'], ['X-Forwarded-For', '2.2.2.2']],
    [['Cache-Control', 'a'], ['Cache-Control', 'b']],
    [['Content-Length', '0'], ['X-After', '1']],
    ...['Age', 'Authorization', 'Content-Length', 'Content-Type', 'ETag', 'Expires', 'From', 'Host',
        'If-Modified-Since', 'If-Unmodified-Since', 'Last-Modified', 'Location', 'Max-Forwards',
        'Proxy-Authorization', 'Referer', 'Retry-After', 'Server', 'User-Agent', 'Origin', 'Expect',
        'If-Match', 'If-None-Match', 'Accept-Encoding', 'Accept-Language', 'Transfer-Encoding',
        'X-Forwarded-Host', 'X-Forwarded-Proto', 'Keep-Alive', 'TE', 'Trailers', 'X-Custom']
        .map((name) => [[name, name === 'Content-Length' ? '0' : 'first'], [name.toUpperCase(), name === 'Content-Length' ? '0' : 'second']]),
];

async function nodeHeaders(lines) {
    return new Promise((resolve) => {
        let settled = false;
        const finish = (value) => {
            if (settled) return;
            settled = true;
            clearTimeout(timer);
            server.closeAllConnections();
            server.close();
            resolve(value);
        };
        const timer = setTimeout(() => finish({ rejected: 'no request event' }), 2000);
        const server = http.createServer((req, res) => {
            res.end();
            finish(Object.entries(req.headers));
        });
        // `Expect` values other than `100-continue` go to `checkExpectation`, not `request`.
        server.on('checkExpectation', (req, res) => {
            res.end();
            finish(Object.entries(req.headers));
        });
        server.on('checkContinue', (req, res) => {
            res.end();
            finish(Object.entries(req.headers));
        });
        // Node's parser rejects some header sets (for example two `Content-Length` lines)
        // with 400 before any handler runs; those vectors record the parser's error code.
        server.on('clientError', (err, socket) => {
            socket.destroy();
            finish({ rejected: err.code });
        });
        server.listen(0, '127.0.0.1', () => {
            const socket = net.connect(server.address().port, '127.0.0.1', () => {
                const hasHost = lines.some(([n]) => n.toLowerCase() === 'host');
                const head = ['GET / HTTP/1.1', ...(hasHost ? [] : ['Host: h']), ...lines.map(([n, v]) => `${n}: ${v}`)];
                socket.write(head.join('\r\n') + '\r\nConnection: close\r\n\r\n');
            });
            socket.on('data', () => {});
            socket.on('error', () => {});
        });
    });
}

// ---------------------------------------------------------------------------------------------
// Unicode property ranges used by es-toolkit's word pattern.

function ranges(regex) {
    const out = [];
    let start = -1;
    for (let cp = 0; cp <= 0x110000; cp++) {
        const inside = cp < 0x110000 && !(cp >= 0xd800 && cp <= 0xdfff) && regex.test(String.fromCodePoint(cp));
        if (inside && start < 0) start = cp;
        if (!inside && start >= 0) {
            out.push([start, cp - 1]);
            start = -1;
        }
    }
    return out;
}

function unicodeTables() {
    const props = [
        ['UPPERCASE_LETTER', /^\p{Lu}$/u, 'General category Lu'],
        ['LOWERCASE_LETTER', /^\p{Ll}$/u, 'General category Ll'],
        ['LETTER', /^\p{L}$/u, 'General category L'],
        ['EMOJI_PRESENTATION', /^\p{Emoji_Presentation}$/u, 'Property Emoji_Presentation'],
        ['EXTENDED_PICTOGRAPHIC', /^\p{Extended_Pictographic}$/u, 'Property Extended_Pictographic'],
    ];
    let text = '//! Unicode property ranges for the es-toolkit word pattern (REQ-21).\n//!\n';
    text += `//! Generated by \`testdata/generate.mjs\` from Node ${process.version} (Unicode ${process.versions.unicode}). Do not edit.\n\n`;
    for (const [name, regex, doc] of props) {
        const list = ranges(regex);
        text += `/// ${doc}, as inclusive code point ranges.\npub(super) const ${name}: &[(u32, u32)] = &[\n`;
        for (let i = 0; i < list.length; i += 4) {
            text += '    ' + list.slice(i, i + 4).map(([a, b]) => `(0x${a.toString(16).toUpperCase()}, 0x${b.toString(16).toUpperCase()})`).join(', ') + ',\n';
        }
        text += '];\n\n';
    }
    writeFileSync(join(here, '../unicode_tables.rs'), text.trimEnd() + '\n');
    console.log('unicode_tables.rs written');
}

write('url.json', urlCases());
write('path_param.json', pathParamCases());
write('query.json', queryCases());
write('kebab.json', kebabCases());
const headers = [];
for (const lines of headerCases) {
    headers.push([lines, await nodeHeaders(lines)]);
}
write('headers.json', headers);
unicodeTables();
