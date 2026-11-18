//! Tool names and input schemas (MCP-07, MCP-10 to MCP-12), ported from the reference's
//! `McpToolRegistryService`.
//!
//! The input comes from an Upstream Service's original document, which can hold any JSON.
//! The port follows the reference's JavaScript wherever that code produces a value, including
//! its oddities (a `$ref` to a string spreads the string's characters into an object). Where
//! the reference throws a `TypeError` instead (a `null` schema or parameter, a
//! non-iterable `parameters`, a `$ref` that is not a string), which makes the whole refresh
//! fail, the port treats the value as absent; those shapes are not supported further.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use super::js;
use super::options::{McpOptions, ParameterFilterContext};

/// Valid JSON Schema type names (MCP-11).
const JSON_SCHEMA_TYPES: [&str; 7] = [
    "string", "number", "integer", "boolean", "object", "array", "null",
];

/// The default tool name (MCP-07): `<service>_<operationId>`, or without an operation id
/// `<service>_<METHOD>_<path>` with `{`, `}` and `/` turned into `_`, runs of `_` collapsed
/// and one leading and one trailing `_` removed.
pub fn default_tool_name(
    service: &str,
    method: &str,
    operation_id: Option<&str>,
    path: &str,
) -> String {
    if let Some(id) = operation_id.filter(|id| !id.is_empty()) {
        return format!("{service}_{id}");
    }
    let mut sanitized = String::with_capacity(path.len());
    for c in path.chars() {
        let c = if matches!(c, '{' | '}' | '/') { '_' } else { c };
        if c == '_' && sanitized.ends_with('_') {
            continue;
        }
        sanitized.push(c);
    }
    let sanitized = sanitized.strip_prefix('_').unwrap_or(&sanitized);
    let sanitized = sanitized.strip_suffix('_').unwrap_or(sanitized);
    format!("{service}_{}_{sanitized}", method.to_uppercase())
}

/// Normalises a schema `type` to JSON Schema (MCP-11): a valid name in any case is
/// lower-cased; a list is normalised entry by entry, keeping the entries that map to a
/// name; a stringified constructor (`function String() { [native code] }`) maps by its name.
/// `None` means the type cannot be mapped.
pub fn normalize_schema_type(value: &Value) -> Option<Value> {
    match value {
        Value::Array(entries) => {
            let names: Vec<Value> = entries
                .iter()
                .filter_map(normalize_schema_type)
                .filter(Value::is_string)
                .collect();
            (!names.is_empty()).then_some(Value::Array(names))
        }
        Value::String(text) => {
            if let Some(name) = constructor_name(text) {
                return constructor_type(name).map(|t| Value::String(t.to_string()));
            }
            let lower = text.to_lowercase();
            JSON_SCHEMA_TYPES
                .contains(&lower.as_str())
                .then_some(Value::String(lower))
        }
        _ => None,
    }
}

/// The name captured by `/^\s*(?:async\s+)?function\*?\s+(\w+)/`.
fn constructor_name(text: &str) -> Option<&str> {
    let rest = text.trim_start_matches(js::is_space);
    let rest = match rest.strip_prefix("async") {
        Some(after) if after.starts_with(js::is_space) => after.trim_start_matches(js::is_space),
        _ => rest,
    };
    let rest = rest.strip_prefix("function")?;
    let rest = rest.strip_prefix('*').unwrap_or(rest);
    if !rest.starts_with(js::is_space) {
        return None;
    }
    let rest = rest.trim_start_matches(js::is_space);
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

fn constructor_type(name: &str) -> Option<&'static str> {
    match name {
        "String" | "Date" => Some("string"),
        "Number" | "BigInt" => Some("number"),
        "Boolean" => Some("boolean"),
        "Array" => Some("array"),
        "Object" => Some("object"),
        _ => None,
    }
}

/// `JSON.stringify(describeType(type))` for a warning.
fn describe_type(value: &Value) -> String {
    Value::String(js::string(value)).to_string()
}

/// Builds input schemas for one service's document, collecting the warnings the reference
/// logs.
pub(crate) struct SchemaBuilder<'a> {
    document: &'a Value,
    /// The port resolves a schema referenced from several places every time (Deviation D8):
    /// a reference counts as a cycle only when it is one of its own ancestors. `false`
    /// reproduces the reference, where every reference resolves once per request body.
    pub(crate) resolve_repeated_refs: bool,
    pub(crate) warnings: Vec<String>,
}

impl<'a> SchemaBuilder<'a> {
    pub(crate) fn new(document: &'a Value) -> Self {
        Self {
            document,
            resolve_repeated_refs: true,
            warnings: Vec::new(),
        }
    }

    /// The input schema of the operation at `path` and `method` (MCP-10, MCP-12).
    pub(crate) fn input_schema(
        &mut self,
        path: &str,
        method: &str,
        service: &str,
        options: &McpOptions,
    ) -> Map<String, Value> {
        let mut properties = Map::new();
        let mut required: Vec<Value> = Vec::new();
        let operation = js::get(self.document, "paths")
            .and_then(|paths| js::get(&paths, path).map(|p| p.into_owned()))
            .and_then(|item| js::get(&item, method).map(|o| o.into_owned()))
            .filter(js::truthy);
        let Some(operation) = operation else {
            let mut schema = Map::new();
            schema.insert("type".into(), json!("object"));
            schema.insert("properties".into(), json!({}));
            schema.insert("required".into(), json!([]));
            return schema;
        };

        if let Some(Value::Array(parameters)) = js::get(&operation, "parameters").as_deref() {
            let context = ParameterFilterContext {
                service_name: service,
                path,
                method,
            };
            for parameter in parameters {
                self.apply_parameter(parameter, &context, options, &mut properties, &mut required);
            }
        }

        if let Some(request_body) = js::get(&operation, "requestBody") {
            let content = js::get(&request_body, "content").filter(|c| js::truthy(c));
            let schema = content
                .as_deref()
                .and_then(|c| js::get(c, "application/json").map(|j| j.into_owned()))
                .and_then(|json| js::get(&json, "schema").map(|s| s.into_owned()))
                .filter(js::truthy);
            if let Some(schema) = schema {
                let resolved = self.resolve(&schema, &mut HashSet::new());
                let description = js::get(&request_body, "description")
                    .filter(|d| js::truthy(d))
                    .map(|d| d.into_owned())
                    .unwrap_or_else(|| json!("Request body"));
                let mut body = js::JsMap::new();
                body.insert("type".to_string(), json!("object"));
                body.insert("description".to_string(), description);
                for (k, v) in resolved {
                    body.insert(k, v);
                }
                properties.insert(
                    "body".into(),
                    Value::Object(body.into_entries().into_iter().collect()),
                );
                if js::truthy_opt(js::get(&request_body, "required").as_deref()) {
                    required.push(json!("body"));
                }
            }
        }

        let mut schema = Map::new();
        schema.insert("type".into(), json!("object"));
        schema.insert("properties".into(), Value::Object(properties));
        if !required.is_empty() {
            schema.insert("required".into(), Value::Array(required));
        }
        schema
    }

    fn apply_parameter(
        &mut self,
        parameter: &Value,
        context: &ParameterFilterContext<'_>,
        options: &McpOptions,
        properties: &mut Map<String, Value>,
        required: &mut Vec<Value>,
    ) {
        if parameter.is_null() {
            return;
        }
        if let Some(filter) = &options.parameter_filter
            && !filter(parameter, context)
        {
            return;
        }
        let location = js::string_opt(js::get(parameter, "in").as_deref());
        let name = js::string_opt(js::get(parameter, "name").as_deref());
        let property_name = format!("{location}_{name}");
        let schema = js::get(parameter, "schema");
        let declared = schema
            .as_deref()
            .and_then(|s| js::get(s, "type").map(|t| t.into_owned()));
        let normalized = declared.as_ref().and_then(normalize_schema_type);
        if let (None, Some(declared)) = (&normalized, &declared)
            && !declared.is_null()
        {
            self.warnings.push(format!(
                "Unmappable type {} for parameter \"{location}.{name}\" ({} {} {}); defaulting to \"string\".",
                describe_type(declared),
                context.service_name,
                context.method,
                context.path
            ));
        }
        let description = js::get(parameter, "description")
            .filter(|d| js::truthy(d))
            .map(|d| d.into_owned())
            .unwrap_or_else(|| Value::String(format!("{location} parameter: {name}")));
        let mut property = Map::new();
        property.insert("type".into(), normalized.unwrap_or_else(|| json!("string")));
        property.insert("description".into(), description);
        if let Some(values) = schema
            .as_deref()
            .and_then(|s| js::get(s, "enum").map(|e| e.into_owned()))
            .filter(js::truthy)
        {
            property.insert("enum".into(), values);
        }
        properties.insert(property_name.clone(), Value::Object(property));
        if js::truthy_opt(js::get(parameter, "required").as_deref()) {
            required.push(Value::String(property_name));
        }
    }

    /// The reference's `resolveSchema`, with Deviation D8 unless
    /// `resolve_repeated_refs` is false.
    pub(crate) fn resolve(
        &mut self,
        schema: &Value,
        refs: &mut HashSet<String>,
    ) -> Map<String, Value> {
        if self.resolve_repeated_refs {
            let mut ancestors = refs.clone();
            self.resolve_inner(schema, &mut ancestors)
        } else {
            self.resolve_inner(schema, refs)
        }
    }

    fn resolve_inner(&mut self, schema: &Value, refs: &mut HashSet<String>) -> Map<String, Value> {
        if let Some(reference) = js::get(schema, "$ref").filter(|r| js::truthy(r)) {
            let Value::String(reference) = reference.as_ref() else {
                return Map::new();
            };
            if !refs.insert(reference.clone()) {
                return Map::new();
            }
            let pointer = reference.replacen("#/", "", 1);
            let mut target = Some(std::borrow::Cow::Borrowed(self.document));
            for segment in pointer.split('/') {
                target = target.and_then(|t| {
                    js::get(&t, segment).map(|v| std::borrow::Cow::Owned(v.into_owned()))
                });
            }
            return match target.filter(|t| js::truthy(t)) {
                Some(target) => {
                    let target = target.into_owned();
                    self.resolve(&target, refs)
                }
                None => Map::new(),
            };
        }

        if let Some(properties) = js::get(schema, "properties").filter(|p| js::truthy(p)) {
            let properties = properties.into_owned();
            let mut resolved = js::spread(schema);
            self.normalize_type_in_place(&mut resolved);
            let mut out = Map::new();
            for (key, value) in js::for_in(&properties) {
                let value = self.resolve(&value, refs);
                out.insert(key, Value::Object(value));
            }
            resolved.insert("properties".into(), Value::Object(js::js_ordered(out)));
            return resolved;
        }

        if let Some(items) = js::get(schema, "items").filter(|i| js::truthy(i)) {
            let items = items.into_owned();
            let mut resolved = js::spread(schema);
            let items = self.resolve(&items, refs);
            resolved.insert("items".into(), Value::Object(items));
            self.normalize_type_in_place(&mut resolved);
            return resolved;
        }

        let mut resolved = js::spread(schema);
        self.normalize_type_in_place(&mut resolved);
        resolved
    }

    fn normalize_type_in_place(&mut self, schema: &mut Map<String, Value>) {
        let Some(declared) = schema.get("type") else {
            return;
        };
        match normalize_schema_type(declared) {
            Some(normalized) => {
                schema.insert("type".into(), normalized);
            }
            None => {
                if !declared.is_null() {
                    self.warnings.push(format!(
                        "Dropping unmappable schema type {} while building MCP input schema.",
                        describe_type(declared)
                    ));
                }
                schema.shift_remove("type");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_names() {
        assert_eq!(
            default_tool_name("users", "get", Some("UserController_show"), "/users/{id}"),
            "users_UserController_show"
        );
        assert_eq!(
            default_tool_name("users", "get", None, "/users/{id}/posts"),
            "users_GET_users_id_posts"
        );
        assert_eq!(default_tool_name("s", "get", Some(""), "/"), "s_GET_");
        assert_eq!(
            default_tool_name("s", "put", None, "//a__{b}//"),
            "s_PUT_a_b"
        );
    }

    #[test]
    fn constructor_names() {
        assert_eq!(
            constructor_name("function String() { [native code] }"),
            Some("String")
        );
        assert_eq!(
            constructor_name("  async   function* Number() {}"),
            Some("Number")
        );
        assert_eq!(constructor_name("function *Number() {}"), None);
        assert_eq!(constructor_name("functionString() {}"), None);
        assert_eq!(constructor_name("asyncfunction X"), None);
        assert_eq!(constructor_name("function\u{a0}Date"), Some("Date"));
    }
}
