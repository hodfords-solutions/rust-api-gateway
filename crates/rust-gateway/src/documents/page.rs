//! The `/documents` page (DOC-01 to DOC-04): the reference's `views/document.hbs` rendered
//! with `getDocumentDetailsForUI()`.
//!
//! The page is the reference's byte for byte except for the script lines that Deviation D6
//! changes:
//!
//! - The source list, the Scalar options and the default source are embedded as JavaScript
//!   string literals holding JSON, with `<`, `>`, `&`, U+2028 and U+2029 escaped, so no
//!   value can end the script or the literal. The reference places raw JSON inside template
//!   literals (`` JSON.parse(`…`) ``), where a backtick, `${`, a backslash escape or
//!   `</script>` in any value breaks the page, and the default source inside a quoted
//!   string with HTML escaping.
//! - `?type=<prefix>` is checked against the sources' `slug` (the reference compares
//!   `doc.name`, which the sources do not have) and the matching source is passed to Scalar
//!   with `default: true`. Without `?type=`, or when it names no source, no source is
//!   marked, as in the reference: its `type` variable (falling back to the last service)
//!   is computed but never given to Scalar, which then opens its own default source. The
//!   `type` lines are kept as the reference has them, comparing `slug`.

use super::json::{JsValue, stringify};

/// `Content-Type` of the page, as Express sends a rendered view.
pub const HTML_UTF8: &str = "text/html; charset=utf-8";

const HEAD: &str = r#"<!doctype html>
<html>
<head>
    <title>API Reference</title>
    <meta charset="utf-8" />
    <meta
            name="viewport"
            content="width=device-width, initial-scale=1" />
    <link rel="icon" href="data:,">
</head>
<body>
<div id="app"></div>
<!-- Load the Script -->
<script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
<!-- Initialize the API Reference -->
<script>
"#;

const OPTIONS: &str = r"        layout: 'modern',
        defaultOpenAllTags: true,
        showOperationId: true,
        hideClientButton: true,
        showSidebar: true,
        showDeveloperTools: 'never',
        showToolbar: 'never',
        operationTitleSource: 'summary',
        theme: 'default',
        persistAuth: true,
        telemetry: true,
        isEditable: false,
        isLoading: false,
        hideModels: false,
        documentDownloadType: 'none',
        hideTestRequestButton: false,
        hideSearch: false,
        hideDarkModeToggle: false,
        withDefaultFonts: true,
        defaultOpenFirstTag: true,
        expandAllModelSections: false,
        expandAllResponses: false,
        orderSchemaPropertiesBy: 'alpha',
        orderRequiredPropertiesFirst: true,
        _integration: 'nestjs',
        default: false,
        agent: {
            disabled: true
        },
        mcp: {
            disabled: true
        },
        ...scalarOptions
    })
</script>
</body>
</html>";

/// The page's sources: `{ title, slug, url: "document-json?type=<prefix>" }` for each
/// service, in the order given (DOC-02).
fn sources(services: &[String]) -> JsValue {
    JsValue::Array(
        services
            .iter()
            .map(|prefix| {
                JsValue::object([
                    ("title", JsValue::string(prefix)),
                    ("slug", JsValue::string(prefix)),
                    (
                        "url",
                        JsValue::string(&format!("document-json?type={prefix}")),
                    ),
                ])
            })
            .collect(),
    )
}

/// `text` as a double-quoted JavaScript string literal that is also safe inside an HTML
/// `<script>` element.
pub(crate) fn script_string_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if u32::from(c) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Renders the page for the loaded services (in the reference's order, see
/// [`page_service_order`](super::page_service_order)) and the application's
/// `scalarOptions`.
pub fn render_page(services: &[String], scalar_options: Option<&serde_json::Value>) -> String {
    let details = stringify(&sources(services));
    // `JSON.stringify(scalarOptions || {})`.
    let options = scalar_options
        .map(JsValue::from_serde)
        .filter(JsValue::is_truthy)
        .unwrap_or_else(|| JsValue::Object(Default::default()));
    let default_doc = script_string_literal(services.last().map_or("", String::as_str));
    let mut page = String::from(HEAD);
    page.push_str(&format!(
        "    const apiDocs = JSON.parse({});\n",
        script_string_literal(&details)
    ));
    page.push_str(&format!(
        "    const scalarOptions = JSON.parse({});\n",
        script_string_literal(&stringify(&options))
    ));
    page.push_str("    const queryString = window.location.search;\n");
    page.push_str("    const urlParams = new URLSearchParams(queryString);\n");
    page.push_str(&format!(
        "    let type = urlParams.get('type') || {default_doc};\n"
    ));
    page.push_str("    if (!apiDocs.some((doc) => doc.slug === type)){\n");
    page.push_str(&format!("        type = {default_doc}\n"));
    page.push_str("    }\n");
    page.push_str("    Scalar.createApiReference('#app', {\n");
    page.push_str(
        "        sources: apiDocs.map((doc) => (doc.slug === urlParams.get('type') ? { ...doc, default: true } : doc)),\n",
    );
    page.push_str(OPTIONS);
    page
}
