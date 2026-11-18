//! The `/document-json` transformation (DOC-05 to DOC-12, DOC-15): the reference's
//! `getDocument`, `removeHeader`, `removeSecuritySchemes` and `changeSecurityOfPath`.
//!
//! Each step mutates the document the way the reference's JavaScript does, so key positions
//! come out the same: assigning an existing key keeps its place, a new key goes last, and a
//! renamed security requirement is deleted and re-added. Where the reference would throw on
//! an unexpected shape, the step leaves that part of the document as it is (Deviation D5).

use super::DocumentOptions;
use super::json::{JsObject, JsString, JsValue};
use crate::request::DEFAULT_SERVICE_PREFIX;

/// Applies the reference's changes to a parsed document of service `prefix`.
pub(crate) fn transform(document: &mut JsObject, prefix: &str, options: &DocumentOptions) {
    add_bearer_scheme(document);
    remove_excluded_headers(document, &options.exclude_headers);
    hide_operations_and_rename_security(document, options);
    if prefix != DEFAULT_SERVICE_PREFIX {
        document.insert(
            key("servers"),
            JsValue::Array(vec![JsValue::object([(
                "url",
                JsValue::string(&format!("/{prefix}")),
            )])]),
        );
    }
}

fn key(name: &str) -> JsString {
    JsString::from_str(name)
}

fn get<'a>(object: &'a JsObject, name: &str) -> Option<&'a JsValue> {
    object.get(&key(name))
}

/// DOC-06, DOC-07, DOC-12: `components.securitySchemes.bearer` is set, then every scheme
/// whose name starts with `auth-` is removed.
///
/// D5: a missing or `null` `components` becomes `{}` (the reference throws). A
/// `securitySchemes` that is missing or falsy becomes `{}`, as in the reference. Any other
/// non-object `components` or `securitySchemes` is left alone: the reference throws for a
/// primitive and silently loses the scheme on an array.
fn add_bearer_scheme(document: &mut JsObject) {
    let components = document.entry(key("components")).or_insert(JsValue::Null);
    if matches!(components, JsValue::Null) {
        *components = JsValue::Object(JsObject::new());
    }
    let JsValue::Object(components) = components else {
        return;
    };
    let schemes = components
        .entry(key("securitySchemes"))
        .or_insert(JsValue::Null);
    if !schemes.is_truthy() {
        *schemes = JsValue::Object(JsObject::new());
    }
    let JsValue::Object(schemes) = schemes else {
        return;
    };
    schemes.insert(
        key("bearer"),
        JsValue::object([
            ("scheme", JsValue::string("bearer")),
            ("bearerFormat", JsValue::string("JWT")),
            ("type", JsValue::string("http")),
        ]),
    );
    schemes.retain(|name, _| !name.starts_with_str("auth-"));
}

/// The operations of every path item, as the reference iterates them: every key of every
/// path item. D5: values that are not objects (path-level `summary`, `$ref`, `parameters`,
/// `servers`, ...) are skipped.
fn operations_mut(paths: &mut JsObject) -> impl Iterator<Item = &mut JsObject> {
    paths
        .values_mut()
        .filter_map(|item| match item {
            JsValue::Object(item) => Some(item),
            _ => None,
        })
        .flat_map(|item| item.values_mut())
        .filter_map(|operation| match operation {
            JsValue::Object(operation) => Some(operation),
            _ => None,
        })
}

fn paths_mut(document: &mut JsObject) -> Option<&mut JsObject> {
    match document.get_mut(&key("paths")) {
        Some(JsValue::Object(paths)) => Some(paths),
        _ => None,
    }
}

/// DOC-08: in every operation, header parameters whose lower-cased name equals an
/// `excludeHeaders` entry are removed. D5: an operation without a `parameters` list, and
/// parameters the reference would throw on (not an object, or a header parameter without
/// a string `name`), are kept.
fn remove_excluded_headers(document: &mut JsObject, exclude_headers: &[String]) {
    let Some(paths) = paths_mut(document) else {
        return;
    };
    for operation in operations_mut(paths) {
        let Some(JsValue::Array(parameters)) = operation.get_mut(&key("parameters")) else {
            continue;
        };
        parameters.retain(|parameter| !is_excluded_header(parameter, exclude_headers));
    }
}

fn is_excluded_header(parameter: &JsValue, exclude_headers: &[String]) -> bool {
    let JsValue::Object(parameter) = parameter else {
        return false;
    };
    let is_header = matches!(get(parameter, "in"), Some(JsValue::String(location)) if location.eq_str("header"));
    let Some(JsValue::String(name)) = get(parameter, "name").filter(|_| is_header) else {
        return false;
    };
    // `excludeHeaders.includes(name.toLowerCase())`: the entry is compared as configured.
    exclude_headers.contains(&name.to_string_lossy().to_lowercase())
}

/// DOC-09, DOC-10: a path item with an operation whose `operationId` is in
/// `hideDocumentIds` is removed (D5: whichever method it is), and in the others every
/// security requirement naming an `openApiSecurityKeys` entry names `bearer` instead.
fn hide_operations_and_rename_security(document: &mut JsObject, options: &DocumentOptions) {
    let Some(paths) = paths_mut(document) else {
        return;
    };
    paths.retain(|_, item| !has_hidden_operation(item, &options.hide_document_ids));
    for operation in operations_mut(paths) {
        let Some(JsValue::Array(requirements)) = operation.get_mut(&key("security")) else {
            continue;
        };
        for from in &options.open_api_security_keys {
            rename_requirement(requirements, from, "bearer");
        }
    }
}

fn has_hidden_operation(item: &JsValue, hide_document_ids: &[String]) -> bool {
    let JsValue::Object(item) = item else {
        return false;
    };
    item.values().any(|operation| match operation {
        JsValue::Object(operation) => matches!(
            get(operation, "operationId"),
            Some(JsValue::String(id)) if hide_document_ids.iter().any(|hidden| id.eq_str(hidden))
        ),
        _ => false,
    })
}

/// The reference's `changeSecurityOfPath`: in each requirement object that names `from`,
/// `from` is deleted and `to` is set to `[]` (in place when `to` is already there, last
/// otherwise). Entries that are not objects are skipped, as the reference's `for…in` does.
fn rename_requirement(requirements: &mut [JsValue], from: &str, to: &str) {
    let from = key(from);
    for requirement in requirements {
        let JsValue::Object(requirement) = requirement else {
            continue;
        };
        if requirement.shift_remove(&from).is_some() {
            requirement.insert(key(to), JsValue::Array(Vec::new()));
        }
    }
}
