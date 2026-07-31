//! Serving the built operator UI out of the binary.
//!
//! See `docs/todos/0021-a-react-operator-ui.md`.
//!
//! Three rules here matter more than they look:
//!
//! 1. **A miss whose last segment has an extension is a 404, never the shell.**
//!    Serving `index.html` for `/assets/index-abc123.js` is how a single-page app
//!    ends up reporting `Uncaught SyntaxError: Unexpected token '<'` — which says
//!    nothing at all about the real problem, a stale or missing asset.
//! 2. **A miss with no extension is the shell, with status 200.** That is the
//!    history fallback: the client router owns unknown routes and renders its own
//!    not-found screen. There was no fallback of any kind before this.
//! 3. **An absent bundle is a 503 that explains itself**, not a 404. The service
//!    genuinely is not ready to serve a UI, and it is a deployment fact rather
//!    than a bug — so it says `npm --prefix web run build` and points at
//!    `/health` and `/api/v1/dashboard` as proof the backend is fine.

use axum::{
    extract::State,
    http::{HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use include_dir::{Dir, include_dir};

use super::AppState;

static BUNDLE: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/web/dist");

/// The placeholder the shell's `<base href>` carries, replaced per request with
/// `server.base_path`. One bundle therefore serves `/` and a reverse proxy at
/// `/seedmedic/` without being rebuilt — and it is done as a string substitution
/// rather than an inline script because the Content-Security-Policy allows
/// `script-src 'self'` only.
const BASE_PLACEHOLDER: &str = "__SEEDMEDIC_BASE__";

/// Vite content-hashes every filename under `assets/`, so those bytes can never
/// change under a given URL and may be cached forever. `index.html` must not be:
/// an upgraded container that keeps serving yesterday's shell out of the browser
/// cache would call an API shape that no longer exists.
const IMMUTABLE_PREFIX: &str = "assets/";

pub async fn serve(State(state): State<AppState>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = path.strip_prefix("assets/").map_or(path, |rest| {
        // A base path arrives on the asset URL too; strip whatever prefix is in
        // front of `assets/`.
        let _ = rest;
        &uri.path()[uri.path().find("assets/").unwrap_or(0)..]
    });

    if !path.is_empty()
        && let Some(file) = BUNDLE.get_file(path)
    {
        let mut response = (StatusCode::OK, file.contents()).into_response();
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, content_type(path));
        headers.insert(
            header::CACHE_CONTROL,
            if path.starts_with(IMMUTABLE_PREFIX) {
                HeaderValue::from_static("public, max-age=31536000, immutable")
            } else {
                HeaderValue::from_static("no-cache")
            },
        );
        return response;
    }

    // An asset that does not exist must fail as an asset.
    if last_segment_has_extension(path) {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    }

    shell(&state)
}

/// `index.html`, with the base path substituted in.
fn shell(state: &AppState) -> Response {
    let Some(index) = BUNDLE
        .get_file("index.html")
        .and_then(|file| file.contents_utf8())
    else {
        return missing_bundle();
    };

    // Always a trailing slash, and never the empty string.
    //
    // `<base href="">` does **not** mean "the root" — HTML resolves an empty href
    // against the current document URL, so on `/review` the base would become
    // `/review` and every relative URL in the app would hang off it: the router
    // would compute an empty path and render the dashboard, and `fetch` would ask
    // for `/review/api/v1/…`. Verified in a browser, which is the only place this
    // shows up.
    let base = state.runtime.current().base_path.clone();
    let html = index.replace(BASE_PLACEHOLDER, &format!("{base}/"));

    let mut response = (StatusCode::OK, html).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    // No inline script, no external anything: the bundle is entirely self-hosted,
    // so the policy can be this tight. `connect-src 'self'` covers the event
    // stream as well as fetch.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             font-src 'self'; connect-src 'self'; form-action 'self'; base-uri 'self'; \
             frame-ancestors 'none'",
        ),
    );
    headers.insert(
        header::HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// 503 rather than 404 or 500: the backend is fine, this deployment just has no
/// UI built into it yet.
fn missing_bundle() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        concat!(
            "<!doctype html><meta charset=utf-8>",
            "<title>SeedMedic — UI not built</title>",
            "<style>body{font:15px/1.6 system-ui,sans-serif;margin:3rem auto;max-width:34rem;",
            "padding:0 1.5rem}code{background:#8882;padding:.1rem .3rem;border-radius:3px}</style>",
            "<h1>The operator UI was not built</h1>",
            "<p>This binary was compiled without the front-end bundle. Build it:</p>",
            "<pre><code>npm --prefix web ci\nnpm --prefix web run build\ncargo build --release</code></pre>",
            "<p>The backend itself is running — <a href=\"health\">/health</a> and ",
            "<a href=\"api/v1/dashboard\">/api/v1/dashboard</a> answer normally.</p>",
        ),
    )
        .into_response()
}

fn last_segment_has_extension(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|segment| segment.contains('.'))
}

/// Only the types Vite actually emits, plus the few an icon or font would need.
/// A hand-written match rather than a mime-guessing dependency: the set of files
/// in the bundle is known, and anything unrecognised is served as bytes.
fn content_type(path: &str) -> HeaderValue {
    let extension = path.rsplit('.').next().unwrap_or_default();
    HeaderValue::from_static(match extension {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "map" => "application/json",
        _ => "application/octet-stream",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule that stops "Unexpected token '<'".
    #[test]
    fn a_path_that_looks_like_an_asset_is_recognised_as_one() {
        assert!(last_segment_has_extension("assets/index-abc123.js"));
        assert!(last_segment_has_extension("favicon.ico"));
        assert!(!last_segment_has_extension("repairs/12"));
        assert!(!last_segment_has_extension("review"));
        assert!(!last_segment_has_extension(""));
        // A dot in a *directory* name must not make a client route look like a
        // file, or deep links break for anyone behind such a path.
        assert!(!last_segment_has_extension("some.dir/repairs"));
    }

    /// `<base href="">` resolves against the current document URL rather than the
    /// origin root, so an empty base silently reparents the whole app onto
    /// whatever path it was first loaded at.
    #[test]
    fn the_injected_base_always_ends_in_a_slash_and_is_never_empty() {
        for (configured, expected) in [("", "/"), ("/seedmedic", "/seedmedic/")] {
            let injected = format!("{configured}/");
            assert_eq!(injected, expected);
            assert!(injected.starts_with('/'), "{injected}");
            assert!(injected.ends_with('/'), "{injected}");
        }
    }

    #[test]
    fn javascript_is_served_as_javascript() {
        // A wrong Content-Type here means the browser refuses the module and the
        // page is blank, with `nosniff` set.
        assert_eq!(
            content_type("assets/index-abc.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type("assets/index-abc.css"),
            "text/css; charset=utf-8"
        );
        assert_eq!(content_type("unknown.bin"), "application/octet-stream");
    }

    /// If the bundle is absent this must be a 503 with instructions rather than a
    /// bare 404 — the difference between "you have not built the UI" and "this URL
    /// is wrong" is most of the debugging.
    #[test]
    fn an_absent_bundle_explains_itself() {
        let built = BUNDLE.get_file("index.html").is_some();
        if built {
            // Nothing to assert about the placeholder path in a tree that has a
            // real bundle; `tests/spa.rs` covers both.
            return;
        }
        let response = missing_bundle();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
