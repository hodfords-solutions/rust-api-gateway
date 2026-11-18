// Generates the golden vectors the `rate_limit` unit tests compare against, by running the
// Reference Gateway's own dependencies under Node:
//
// - `trust proxy` compilation (Express `compileTrust`) and `request.ip` (`proxy-addr`),
// - `normalizeIp` and `ttlToHumanReadable` from `@hodfords/nestjs-api-gateway`.
//
// Run from the repository root after `pnpm install` in `conformance/reference`:
//
//     node crates/rust-gateway/src/rate_limit/testdata/generate.mjs
//
// The output, `golden.json` next to this script, is committed.

import { createRequire } from 'node:module';
import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const reference = join(here, '../../../../../conformance/reference');
const requireFromReference = createRequire(join(reference, 'package.json'));
const requireFromExpress = createRequire(requireFromReference.resolve('express'));

const { compileTrust } = requireFromExpress('./lib/utils.js');
const proxyaddr = requireFromExpress('proxy-addr');
const gatewayRoot = dirname(requireFromReference.resolve('@hodfords/nestjs-api-gateway/package.json'));
const { normalizeIp } = await import(pathToFileURL(join(gatewayRoot, 'throttlers/helpers/ip.helper.js')).href);
const { ttlToHumanReadable } = await import(pathToFileURL(join(gatewayRoot, 'restful/helpers/string.helper.js')).href);
const versions = {
    node: process.version,
    express: requireFromExpress('./package.json').version,
    'proxy-addr': requireFromExpress('proxy-addr/package.json').version,
    'ipaddr.js': createRequire(requireFromExpress.resolve('proxy-addr'))('ipaddr.js/package.json').version,
    '@hodfords/nestjs-api-gateway': requireFromReference('@hodfords/nestjs-api-gateway/package.json').version
};

// ---------------------------------------------------------------------------------------------
// normalizeIp
// ---------------------------------------------------------------------------------------------

const ipInputs = [
    '', 'unknown', 'garbage', ' 1.2.3.4', '1.2.3.4 ', '1.2.3.4%eth0',
    '203.0.113.7', '0.0.0.0', '255.255.255.255', '256.1.1.1', '01.2.3.4', '1.2.3', '1.2.3.4.5', '127.1',
    '::', '::1', '::ffff:203.0.113.7', '::FFFF:203.0.113.7', '::ffff:cb00:7107', '0:0:0:0:0:ffff:203.0.113.7',
    '::ffff:0.0.0.0', '::ffff:255.255.255.255', '::fffe:1.2.3.4', '0::ffff:1.2.3.4', '::ffff:01.2.3.4',
    '2001:db8:1:2:aaaa:bbbb:cccc:dddd', '2001:db8:1:2::1', '2001:0DB8:0001:0002:ffff:ffff:ffff:fffe',
    '2001:db8:1:3::1', '2001:db8:1:2ff::1', 'fe80::', 'fe80::1%eth0', 'fe80::1%', 'fe80::1%%', '%eth0',
    '64:ff9b::203.0.113.7', '1:2:3:4:5:6:7:8', '1:2:3:4:5:6:7:8:9', '1:2:3:4:5:6:7', '1:2:3:4:5:6:7::',
    '::2:3:4:5:6:7:8', '1::8', '1:2:3:4:5:6::8', ':::', '1:::2', '::1::', '00001::', '1:2:3:4:5:6:1.2.3.4',
    '1:2:3:4:5::1.2.3.4', '1:2:3:4:5:6:7:1.2.3.4', 'ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff', 'FE80::ABCD',
    '[::1]', '::1/128', 'g::1', '2001:db8::1 ', ' ::1', '::ffff:1.2.3', '1.2.3.4:80'
];
const subnets = [undefined, 1, 7, 15, 16, 17, 48, 56, 64, 96, 100, 127, 128];
const normalize = [];
for (const input of ipInputs) {
    for (const subnet of subnets) {
        normalize.push({ input, subnet: subnet ?? null, output: subnet === undefined ? normalizeIp(input) : normalizeIp(input, subnet) });
    }
}

// ---------------------------------------------------------------------------------------------
// trust proxy compilation
// ---------------------------------------------------------------------------------------------

// Each value is what an application passes to `app.set('trust proxy', value)`.
const trustValues = [
    false, true, 0, 1, 2, 3,
    'loopback', 'linklocal', 'uniquelocal', 'loopback, linklocal, uniquelocal', ' loopback ,uniquelocal ',
    'loopback,\t10.0.0.1\n', 'loopback, uniquelocal﻿',
    ['loopback', 'linklocal', 'uniquelocal'], [], ['loopback', 'loopback'],
    '10.0.0.0/8', '10.0.0.0/255.0.0.0', '10.1.2.3/255.255.255.255', '127.0.0.1', '::1', '::1/128',
    ['::ffff:127.0.0.1'], ['::ffff:0:0/96'], ['::ffff:7f00:0/104'], ['::ffff:127.0.0.0/104'],
    ['2001:db8::/32'], 'fe80::/10, 169.254.0.0/16', ['fc00::/7', '192.168.0.0/16'], ['203.0.113.0/24'],
    ['127.1'], ['0x7f.0.0.1/8'], ['2130706433'], ['0177.0.0.1'], ['fe80::%eth0/10'], ['1.2.3.4/32'],
    ['1.2.3.4/1'], ['::/1'], ['10.0.0.0/08'], ['::ffff:10.0.0.1'],
    // Invalid values: compilation throws.
    '', 'foo', 'loopback, foo', '10.0.0.0/0', '10.0.0.0/33', '::1/129', '::1/0', '10.0.0.0/255.0.255.0',
    '10.0.0.0/abc', '10.0.0.0/', '/8', ['loopback '], [' 127.0.0.1'], ['10.0.0.0/-1'], ['10.0.0.0/1.5'],
    ['::1/ffff::'], ['1.2.3.4/::'], ['256.0.0.0'], ['10.0.0.0/+8'], ['LOOPBACK']
];

const trustCompiled = [];
const trustFns = [];
for (const value of trustValues) {
    try {
        const fn = compileTrust(value);
        trustCompiled.push({ trust: value, ok: true });
        trustFns.push([value, fn]);
    } catch (error) {
        trustCompiled.push({ trust: value, ok: false, error: String(error.message) });
    }
}

// ---------------------------------------------------------------------------------------------
// request.ip
// ---------------------------------------------------------------------------------------------

// Peer addresses as Node reports them for a dual-stack listener, written the way Rust's
// `IpAddr` Display writes them (the port formats the peer with Display).
const peers = [
    null, '::ffff:127.0.0.1', '127.0.0.1', '::1', '::ffff:10.1.2.3', '10.1.2.3', '::ffff:203.0.113.9',
    '203.0.113.9', 'fe80::1', 'fc00::1', '2001:db8::5', '::ffff:169.254.3.4', '::ffff:192.168.1.1', '::'
];
const forwardedFor = [
    null, '', ' ', ',', ', ,', '1.2.3.4', '1.2.3.4, 10.0.0.1', '203.0.113.5, 127.0.0.1', 'garbage',
    'garbage, 10.0.0.2', ' 1.2.3.4 ,  5.6.7.8 ', '1.2.3.4,,5.6.7.8', 'a b, c', 'a  b', '::1',
    'fe80::1%eth0, 10.0.0.1', '127.1', '127.1, 127.0.0.2', '0x7f.0.0.1', '2130706433, 10.0.0.9',
    '0177.0.0.1', '::ffff:10.0.0.3', '::ffff:7f00:1', '1:2:3:4:5:6:7:8::', '1:2:3:4:5:6:7:8::, ::1',
    'fc00::1, fd12::2', '169.254.1.1', '8.8.8.8, 172.16.5.4, 192.168.0.9', '1.2.3.4\t, 10.0.0.1',
    '9.9.9.9, 10.0.0.1, 10.0.0.2, 10.0.0.3', '2001:db8::1, ::ffff:127.0.0.1', '[::1], 10.0.0.1',
    '1.2.3.4:5678, 10.0.0.1', 'unknown', '10.0.0.1, unknown', 'é, 10.0.0.1', '1.2.3.4 , 5.6.7.8,'
];

// One table per trust value: `ips[p][x]` is `request.ip` for `peers[p]` and `forwardedFor[x]`.
const clientAddress = [];
for (const [trust, fn] of trustFns) {
    const ips = peers.map((peer) =>
        forwardedFor.map((xff) => {
            const req = {
                socket: { remoteAddress: peer ?? undefined },
                headers: xff === null ? {} : { 'x-forwarded-for': xff }
            };
            return proxyaddr(req, fn) ?? null;
        })
    );
    clientAddress.push({ trust, ips });
}

// ---------------------------------------------------------------------------------------------
// ttlToHumanReadable
// ---------------------------------------------------------------------------------------------

const ttls = [
    0, 0.5, 1, 59, 59.999, 60, 61.5, 119, 119.999, 120, 179, 180, 3599, 3599.999, 3600, 5400, 7199, 7200,
    7259, 10799, 10800, 86400, 1e6, 1e21 * 60 * 60, -1, -60, -3600
];
const ttlText = ttls.map((ttl) => ({ ttl, text: ttlToHumanReadable(ttl) }));

const golden = {
    generatedBy: 'crates/rust-gateway/src/rate_limit/testdata/generate.mjs',
    versions,
    normalize,
    trustCompiled,
    peers,
    forwardedFor,
    clientAddress,
    ttlText
};
writeFileSync(join(here, 'golden.json'), JSON.stringify(golden) + '\n');
console.log(
    `normalize: ${normalize.length}, trustCompiled: ${trustCompiled.length}, clientAddress: ${clientAddress.length} tables, ttlText: ${ttlText.length}`
);
