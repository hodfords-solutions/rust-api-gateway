/**
 * The MCP callbacks (`filter`, `parameterFilter`, `toolName`) are functions, so the
 * conformance configuration file describes them declaratively and this file turns the
 * description into the functions the Reference Gateway calls. `rust-gateway-server` builds
 * the same callbacks from the same fields. Documented in `conformance/README.md`.
 */
import type { McpOption, RouterDetail } from '@hodfords/nestjs-api-gateway';
import type { ConformanceConfig } from './config.js';

/** `filter`: a Route is rejected when its service or its OpenAPI path matches. */
export type McpFilterSpec = {
    rejectServices?: string[];
    rejectPathPrefixes?: string[];
};

/** `parameterFilter`: a parameter is dropped when its `name` or its `in` is listed. */
export type McpParameterFilterSpec = {
    rejectNames?: string[];
    rejectIn?: string[];
};

/**
 * `toolName`: a template in which `{serviceName}`, `{method}`, `{operationId}`, `{path}` and
 * `{defaultName}` are replaced by the context values (an absent operation id is the empty
 * string). Every occurrence is replaced; other text is kept as is.
 */
export function applyToolNameTemplate(
    template: string,
    context: { serviceName: string; method: string; operationId?: string; path: string; defaultName: string }
): string {
    const values: Record<string, string> = {
        serviceName: context.serviceName,
        method: context.method,
        operationId: context.operationId ?? '',
        path: context.path,
        defaultName: context.defaultName
    };
    return template.replace(/\{(serviceName|method|operationId|path|defaultName)\}/g, (_, key: string) => values[key]);
}

export function mcpOption(mcp: ConformanceConfig['mcp']): McpOption | undefined {
    if (!mcp) return undefined;
    const { filter, parameterFilter, toolName, ...option } = mcp;
    const result: McpOption = { ...option };
    if (filter) {
        result.filter = (serviceName: string, route: RouterDetail): boolean =>
            !(filter.rejectServices ?? []).includes(serviceName) &&
            !(filter.rejectPathPrefixes ?? []).some((prefix) => route.path.startsWith(prefix));
    }
    if (parameterFilter) {
        result.parameterFilter = (param: any): boolean =>
            !(parameterFilter.rejectNames ?? []).includes(param?.name) &&
            !(parameterFilter.rejectIn ?? []).includes(param?.in);
    }
    if (toolName !== undefined) {
        result.toolName = (context) => applyToolNameTemplate(toolName, context);
    }
    return result;
}
