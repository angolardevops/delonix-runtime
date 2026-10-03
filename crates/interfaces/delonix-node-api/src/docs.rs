//! `GET /docs` (Swagger UI) and `GET /redoc` (ReDoc) — the API's documentation
//! served by the API itself, in the style of FastAPI (ADR-0042 D3).
//!
//! Both pages render `GET /openapi.json`, the document the contract gate
//! generates from `proto/`. Every byte they load comes from this socket: the UI
//! files are embedded in the binary (`third_party/node-api-docs/`, byte-identical
//! to the upstream npm packages and checked against its `SHA256SUMS` by
//! `build.rs`), and the
//! Content-Security-Policy each response carries forbids anything else — a
//! node may be offline, and a page that drives the engine's API must not load a
//! script from somewhere else. Swagger UI's online validator (a request to
//! `validator.swagger.io` by default) is turned off.
//!
//! Measured in a browser (2026-10-02, through a local TCP relay to the socket):
//! both pages render the document with no console message, Swagger UI's "Try
//! it out" reaches the socket, and ReDoc's search runs in its `blob:` worker.
//! The one console line is the policy at work: ReDoc's footer points its logo
//! at `https://cdn.redoc.ly/…`, and the browser blocks it before any request
//! leaves.
//!
//! Reaching the pages from a browser is the same path as the API: a
//! port-forward or an SSH tunnel to the socket (ADR-0042 D5).

use axum::extract::Path;
use axum::response::{IntoResponse, Response};
use hyper::header::{HeaderValue, CACHE_CONTROL, CONTENT_TYPE};
use hyper::StatusCode;

/// One embedded file: its name under `/docs/assets/`, its media type and its
/// bytes.
struct Asset {
    name: &'static str,
    mime: &'static str,
    bytes: &'static [u8],
}

macro_rules! asset {
    ($name:literal, $mime:literal, $path:literal) => {
        Asset {
            name: $name,
            mime: $mime,
            bytes: include_bytes!(concat!("../../../../third_party/node-api-docs/", $path)),
        }
    };
}

/// Every embedded file the pages load, and the license texts that ship with
/// them. A name not listed here is a 404.
const ASSETS: &[Asset] = &[
    asset!(
        "swagger-ui-bundle.js",
        "text/javascript; charset=utf-8",
        "swagger-ui/swagger-ui-bundle.js"
    ),
    asset!(
        "swagger-ui.css",
        "text/css; charset=utf-8",
        "swagger-ui/swagger-ui.css"
    ),
    asset!(
        "swagger-ui-LICENSE",
        "text/plain; charset=utf-8",
        "swagger-ui/LICENSE"
    ),
    asset!(
        "swagger-ui-NOTICE",
        "text/plain; charset=utf-8",
        "swagger-ui/NOTICE"
    ),
    asset!(
        "swagger-ui-bundle.js.LICENSE.txt",
        "text/plain; charset=utf-8",
        "swagger-ui/swagger-ui-bundle.js.LICENSE.txt"
    ),
    asset!(
        "redoc.standalone.js",
        "text/javascript; charset=utf-8",
        "redoc/redoc.standalone.js"
    ),
    asset!(
        "redoc-LICENSE",
        "text/plain; charset=utf-8",
        "redoc/LICENSE"
    ),
    asset!(
        "redoc.standalone.js.LICENSE.txt",
        "text/plain; charset=utf-8",
        "redoc/redoc.standalone.js.LICENSE.txt"
    ),
];

/// Swagger UI's start-up, a file of its own so the policy can keep scripts to
/// `'self'` with no inline code. `validatorUrl: null` keeps the page from
/// sending the document to the online validator.
const SWAGGER_INIT: &str = r##"window.addEventListener("load", function () {
  window.ui = SwaggerUIBundle({
    url: "/openapi.json",
    dom_id: "#swagger-ui",
    deepLinking: true,
    validatorUrl: null,
  });
});
"##;

const SWAGGER_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>delonix.node.v1 — Swagger UI</title>
<link rel="stylesheet" href="/docs/assets/swagger-ui.css">
</head>
<body>
<div id="swagger-ui"></div>
<script src="/docs/assets/swagger-ui-bundle.js"></script>
<script src="/docs/assets/swagger-init.js"></script>
</body>
</html>
"#;

const REDOC_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>delonix.node.v1 — ReDoc</title>
</head>
<body>
<redoc spec-url="/openapi.json"></redoc>
<script src="/docs/assets/redoc.standalone.js"></script>
</body>
</html>
"#;

/// The policy every docs response carries: scripts, styles, fonts and requests
/// only from this socket. The two UIs set styles at run time (`'unsafe-inline'`
/// for styles only), ReDoc's search runs in a worker it builds from a `blob:`,
/// and both draw small images from `data:` URLs.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
     style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; \
     connect-src 'self'; worker-src blob:; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'";

fn respond(mime: &'static str, body: impl Into<axum::body::Body>) -> Response {
    let mut res = (StatusCode::OK, body.into()).into_response();
    let h = res.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    // The files change only with the binary; a browser asks again each time
    // rather than keep a copy of an older engine's UI.
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    res
}

/// `GET /docs`.
pub async fn swagger() -> Response {
    respond("text/html; charset=utf-8", SWAGGER_PAGE)
}

/// `GET /redoc`.
pub async fn redoc() -> Response {
    respond("text/html; charset=utf-8", REDOC_PAGE)
}

/// `GET /docs/assets/{name}`: an embedded file, or 404.
pub async fn asset(Path(name): Path<String>) -> Response {
    if name == "swagger-init.js" {
        return respond("text/javascript; charset=utf-8", SWAGGER_INIT);
    }
    match ASSETS.iter().find(|a| a.name == name) {
        Some(a) => respond(a.mime, a.bytes),
        None => (
            StatusCode::NOT_FOUND,
            format!("no docs asset named '{name}'"),
        )
            .into_response(),
    }
}

/// The names under `/docs/assets/` (the tests check every page reference
/// resolves to one).
pub fn asset_names() -> Vec<&'static str> {
    let mut v: Vec<&str> = ASSETS.iter().map(|a| a.name).collect();
    v.push("swagger-init.js");
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `/docs/assets/…` a page references is served, and nothing a page
    /// loads points anywhere else.
    #[test]
    fn the_pages_load_only_what_this_socket_serves() {
        let names = asset_names();
        for page in [SWAGGER_PAGE, REDOC_PAGE] {
            for part in page.split(['"', '\'']) {
                if let Some(name) = part.strip_prefix("/docs/assets/") {
                    assert!(names.contains(&name), "{name} is referenced and not served");
                }
                assert!(
                    !part.starts_with("http://")
                        && !part.starts_with("https://")
                        && !part.starts_with("//"),
                    "a page loads something from elsewhere: {part}"
                );
            }
        }
        assert!(SWAGGER_INIT.contains("validatorUrl: null"));
        assert!(SWAGGER_INIT.contains("\"/openapi.json\""));
        assert!(REDOC_PAGE.contains("spec-url=\"/openapi.json\""));
    }

    #[test]
    fn the_policy_keeps_scripts_to_this_socket() {
        let script = CONTENT_SECURITY_POLICY
            .split(';')
            .map(str::trim)
            .find(|d| d.starts_with("script-src"))
            .expect("script-src");
        assert_eq!(script, "script-src 'self'");
        assert!(CONTENT_SECURITY_POLICY.contains("default-src 'none'"));
        assert!(CONTENT_SECURITY_POLICY.contains("connect-src 'self'"));
    }

    #[test]
    fn the_embedded_files_are_the_upstream_ones() {
        // The real check is build.rs against third_party/node-api-docs/SHA256SUMS; this pins the shape a
        // truncated or empty copy would break.
        let js = ASSETS
            .iter()
            .find(|a| a.name == "swagger-ui-bundle.js")
            .unwrap();
        assert!(js.bytes.len() > 1_000_000, "{}", js.bytes.len());
        let redoc = ASSETS
            .iter()
            .find(|a| a.name == "redoc.standalone.js")
            .unwrap();
        assert!(redoc.bytes.len() > 1_000_000, "{}", redoc.bytes.len());
        let lic = ASSETS
            .iter()
            .find(|a| a.name == "swagger-ui-LICENSE")
            .unwrap();
        assert!(std::str::from_utf8(lic.bytes)
            .unwrap()
            .contains("Apache License"));
    }
}
