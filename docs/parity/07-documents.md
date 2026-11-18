# 07 Document endpoints

Two endpoints, present only when documents are enabled (CFG-14). Documents are served per Upstream Service; there is no merged document.

## `GET /documents`

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| DOC-01 | Returns an HTML page that loads the Scalar API reference from `https://cdn.jsdelivr.net/npm/@scalar/api-reference` and initialises it in the element `#app`. | `views/document.hbs:1-63`; `restful/controllers/document.controller.ts:15-22` | C | todo |
| DOC-02 | The page lists one source per Upstream Service that has a loaded document: `{ title: <prefix>, slug: <prefix>, url: "document-json?type=<prefix>" }`, in load order. | `restful/services/open-api.service.ts:169-185` | C, U | todo |
| DOC-03 | The page passes Scalar the fixed option set in the reference template, then spreads `scalarOptions` over it so the application's values win. | `views/document.hbs:25-60` | C | todo |
| DOC-04 | **Deviation D6.** `?type=<prefix>` selects that service's document, falling back to the last loaded service. The source list and options are embedded so that no value can break out of the script. Reference: the selection compares a field the sources do not have, so it never matches; the JSON is placed unescaped inside a template literal. | `views/document.hbs:17-24` | R | todo |

## `GET /document-json?type=<prefix>`

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| DOC-05 | Fetches the named service's document from its document URL on every call and returns it as JSON after the changes below. Everything not mentioned is returned as the upstream sent it, including `x-rate-limits` and other extensions. | `restful/services/open-api.service.ts:187-221`; `restful/controllers/document.controller.ts:24-28` | C | todo |
| DOC-06 | Adds `components.securitySchemes.bearer = { "scheme": "bearer", "bearerFormat": "JWT", "type": "http" }`. | `restful/services/open-api.service.ts:189-196` | C, U | todo |
| DOC-07 | Removes every security scheme whose name starts with `auth-`. | `restful/services/open-api.service.ts:238-244` | C, U | todo |
| DOC-08 | In every operation, removes header parameters whose lower-cased name equals an `excludeHeaders` entry. The entry is compared as configured, so only lower-case entries take effect. | `restful/services/open-api.service.ts:223-236` | C, U | todo |
| DOC-09 | In every operation's security requirements, each name listed in `openApiSecurityKeys` is replaced by `bearer` with an empty scope list. | `restful/services/open-api.service.ts:207-210,246-255` | C, U | todo |
| DOC-10 | A path that has an operation whose id is in `hideDocumentIds` is removed entirely, with all its methods. **Deviation D5:** this never fails. Reference: 500 when the hidden operation is not the last method of its path. | `restful/services/open-api.service.ts:199-206` | C, R | todo |
| DOC-11 | Sets `servers` to `[{ "url": "/<prefix>" }]`, except for the Default Service, whose `servers` is left as received. | `restful/services/open-api.service.ts:213-219` | C, U | todo |
| DOC-12 | The bearer scheme of DOC-06 is added before DOC-07 runs and is never removed by it. | `restful/services/open-api.service.ts:192-197` | U | todo |
| DOC-13 | When the upstream document cannot be fetched (a non-2xx answer or a closed connection), the endpoint answers 500 with `{"statusCode":500,"message":"Internal server error"}`. | `restful/services/open-api.service.ts:188` | C (`documents::doc_13_document_fetch_fails`) | todo |
| DOC-14 | **Deviation D5.** A missing `type`, or one that names no service with a loaded document, answers 404. Reference: 500. | `restful/services/open-api.service.ts:188` | R | todo |
| DOC-15 | **Deviation D5.** An operation without `parameters`, a document without `components`, and path-item keys that are not operations are all handled without error. Reference: 500 in each case. | `restful/services/open-api.service.ts:189,223-236` | R | todo |
