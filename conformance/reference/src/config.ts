import { readFileSync } from 'node:fs';
import type { ConnectionOptions } from 'node:tls';
import type { McpFilterSpec, McpParameterFilterSpec } from './mcp-callbacks.js';

/**
 * The conformance configuration file. Its path comes from `GATEWAY_CONFIG`.
 *
 * The same file is read by `rust-gateway-server`, so every field here must mean the same
 * thing to both targets. See `conformance/README.md` for the full description.
 */
export type ConformanceConfig = {
    port: number;
    trustProxy?: boolean | number | string | string[];
    apiServices: { prefix: string; docUrl: string; host: string; directPrefixes?: string[] }[];
    excludeHeaders: string[];
    openApiSecurityKeys: string[];
    openApiSecurityApiKeys?: string[];
    throttler: {
        globalIpRateLimit: number;
        globalIpRateLimitTTL: number;
        globalCustomRateLimit: number;
        globalCustomRateLimitTTL: number;
        isEnable: boolean;
        anonymousIpRateLimit?: number;
        anonymousIpRateLimitTTL?: number;
        ipv6Subnet?: number;
        /** `"protocol"` installs the test Identity resolver; absent means no resolver. */
        keyResolver?: 'protocol';
    };
    restful: { isEnableDocument: boolean; hideDocumentIds?: string[] };
    scalarOptions?: unknown;
    redis: {
        host: string;
        port: number;
        db?: number;
        username?: string;
        password?: string;
        tls?:
            | boolean
            | {
                  caFile?: string;
                  certFile?: string;
                  keyFile?: string;
                  servername?: string;
                  rejectUnauthorized?: boolean;
              };
    };
    /** Passed to undici's `Pool` unchanged (CFG-10). */
    pool: Record<string, unknown>;
    bypassRoutePrefixes?: string[];
    mcp?: {
        enabled: boolean;
        allowedServices?: string[];
        allowedOperations?: string[];
        serverInfo?: { name?: string; version?: string };
        /** Declarative stand-ins for the MCP callbacks; see `mcp-callbacks.ts`. */
        filter?: McpFilterSpec;
        parameterFilter?: McpParameterFilterSpec;
        toolName?: string;
    };
    /**
     * Harness field: the host application's `http.Server` timeouts, in milliseconds.
     * Absent fields keep Node's defaults.
     */
    server?: {
        headersTimeout?: number;
        requestTimeout?: number;
        keepAliveTimeout?: number;
        keepAliveTimeoutBuffer?: number;
        connectionsCheckingInterval?: number;
    };
};

function loadConfig(): ConformanceConfig {
    const path = process.env.GATEWAY_CONFIG;
    if (!path) {
        throw new Error('GATEWAY_CONFIG must name the conformance configuration file');
    }
    const config = JSON.parse(readFileSync(path, 'utf8')) as ConformanceConfig;

    // Environment overrides, named like the Reference Gateway sample (EX-01).
    if (process.env.APP_PORT) config.port = Number(process.env.APP_PORT);
    if (process.env.REDIS_HOST) config.redis.host = process.env.REDIS_HOST;
    if (process.env.REDIS_PORT) config.redis.port = Number(process.env.REDIS_PORT);
    if (process.env.REDIS_DB) config.redis.db = Number(process.env.REDIS_DB);
    return config;
}

export const config = loadConfig();

/** Converts the file's TLS description into Node `tls.connect` options. */
export function redisTls(): boolean | ConnectionOptions | undefined {
    const tls = config.redis.tls;
    if (tls === undefined || tls === false) return undefined;
    if (tls === true) return true;
    return {
        ...(tls.caFile ? { ca: readFileSync(tls.caFile) } : {}),
        ...(tls.certFile ? { cert: readFileSync(tls.certFile) } : {}),
        ...(tls.keyFile ? { key: readFileSync(tls.keyFile) } : {}),
        ...(tls.servername ? { servername: tls.servername } : {}),
        ...(tls.rejectUnauthorized === false ? { rejectUnauthorized: false } : {})
    };
}
