//! The operator interface: a driving adapter over the repair capability.
//!
//! Server-rendered, no JavaScript, no API surface beyond what the pages need.
//! It reads repair state and performs the review actions; it contains no
//! rules of its own, because a decision the UI could make differently from the
//! worker is a decision in the wrong place.
//!
//! The first sentence is being replaced by
//! `docs/todos/0021-a-react-operator-ui.md`: a React client in `web/` over a
//! JSON API under `api/`. The rest of it holds, and gets sharper — the client
//! is a separate program that could disagree, so it does no validation and
//! re-derives no rule this crate can send it.

pub mod api;
pub(crate) mod error;
mod health;
pub(crate) mod jobs;
mod layout;
mod login;
#[cfg(feature = "metrics")]
mod metrics;
pub(crate) mod review;
mod settings;
pub mod spa;
mod status;

pub use layout::Chrome;

use std::{net::SocketAddr, sync::Arc};

use axum::{
    Router,
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};

use crate::runtime::RuntimeHandle;

/// One generation lives on `runtime` and is fetched fresh — `runtime.current()`
/// — at the top of every handler, so a request always sees a consistent
/// snapshot even if a reload lands mid-request.
#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<RuntimeHandle>,
    /// What the process is actually listening on, fixed for its lifetime.
    /// Unlike everything on `Runtime`, a reload can never replace this — so
    /// `server.bind_address` is reported as needing a restart instead of
    /// being silently ignored.
    pub bind_address: SocketAddr,
}

pub fn router(runtime: Arc<RuntimeHandle>, bind_address: SocketAddr) -> Router {
    let state = AppState {
        runtime,
        bind_address,
    };

    // `/` is the React shell, reached through the fallback at the bottom of this
    // function. `/status` and `/jobs/{id}` stay as server-rendered pages until
    // 0021's cutover deletes them; their JSON equivalents already exist and are
    // what the SPA uses, so nothing depends on them but their own tests.
    let router = Router::new()
        .route("/status", get(status::page))
        .route("/jobs/{id}", get(jobs::detail))
        .route("/login", get(login::show).post(login::submit))
        .route("/logout", post(login::logout));

    #[cfg(feature = "metrics")]
    let router = router.route("/metrics", get(metrics::handler));

    // `/api/v1` alongside the maud pages while the SPA is built — see
    // docs/todos/0021-a-react-operator-ui.md's sequence. The open half is merged
    // *outside* the auth layer below, so exemption is structural.
    let router = router.nest("/api/v1", api::router());

    router
        .route("/jobs/{id}/retry", post(review::retry))
        .route("/jobs/{id}/restart", post(review::restart))
        .route("/jobs/{id}/abandon", post(review::abandon))
        .route(
            "/jobs/{id}/abandon-and-discard",
            post(review::abandon_and_discard),
        )
        .route("/jobs/{id}/approve-resume", post(review::approve_resume))
        .route(
            "/jobs/{id}/choose-candidate",
            post(review::choose_candidate),
        )
        .route("/jobs/bulk/retry", post(review::bulk_retry))
        .route("/jobs/bulk/abandon", post(review::bulk_abandon))
        .merge(settings::router())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_auth_token,
        ))
        // Everything below this line is reachable without a credential, by
        // being registered outside the layer rather than by being named in a
        // list the middleware has to consult. `/health` is the container's
        // readiness probe; `/api/v1/session` is how the SPA discovers whether
        // there is anything to sign in to.
        .route("/health", get(health::health))
        .nest("/api/v1", api::open_router())
        // The SPA shell and its assets, also outside the auth layer.
        //
        // Deliberate, and a change of posture from
        // docs/todos/0018-browser-usable-authentication.md's "only /health and
        // /login are readable": the bundle contains no operator data and no
        // secret — the plaintext-accessor grep below and the sentinel tests
        // guarantee that separately — and guarding it produces a redirect loop.
        // `/` would send an
        // unauthenticated browser to `/login`, which is a *client* route served by
        // the same shell, whose own request for `/assets/…` would then 401. The
        // result is a blank page with no way in. Every `/api/v1` route stays
        // guarded, which is where the data actually is.
        .fallback(get(spa::serve))
        .with_state(state)
}

/// No-op when `server.auth_token` is unset — the documented default posture
/// is "do not expose this to the internet," not "this is secure." `/health`
/// and `/login` are exempt; nothing else is, including `/settings`.
///
/// Two credentials are accepted, checked in that order: an `Authorization:
/// Bearer` header (for scripts, unchanged since `docs/todos/0011`) or a
/// session cookie minted by `/login` (for a browser, which cannot attach a
/// bearer header from an HTML form — see
/// `docs/todos/0018-browser-usable-authentication.md`). A bearer attempt that
/// fails is always a plain 401: a script that starts silently following a
/// redirect to a login page instead of failing is worse than one that gets a
/// 401. Missing credentials get content-negotiated instead: a browser
/// (`Accept` contains `text/html`) is sent to `/login`; anything else gets a
/// 401.
async fn require_auth_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(request.uri().path(), "/health" | "/login") {
        return next.run(request).await;
    }

    let runtime = state.runtime.current();
    let Some(expected) = runtime.auth_token.as_ref() else {
        return next.run(request).await;
    };

    if let Some(header) = request.headers().get(header::AUTHORIZATION) {
        let ok = header
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| expected.verify(token));
        return if ok {
            next.run(request).await
        } else {
            (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response()
        };
    }

    let authenticated = login::session_id_from(request.headers())
        .is_some_and(|session_id| state.runtime.has_session(&session_id));
    if !authenticated {
        // A request under `/api/` is **always** 401, never a redirect, whatever it
        // says it accepts. `fetch` follows a 3xx transparently, so a redirect here
        // lands the client on the login page and hands it HTML to parse as JSON —
        // a `SyntaxError: Unexpected token '<'` that says nothing about the real
        // problem. The content negotiation below stays for the server-rendered
        // pages, which do want a browser sent somewhere it can type a token.
        return if is_api(&request) {
            (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response()
        } else if wants_html(&request) {
            Redirect::to("/login").into_response()
        } else {
            (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response()
        };
    }

    // `SameSite=Strict` is the primary CSRF control and is load-bearing, not
    // decoration (see the cookie built in `login::cookie_header`) — but it is
    // a same-*browser* control, not a same-*origin* one, so a request that
    // does carry same-browser authority (this cookie) is also checked here.
    // Only reachable once a token is configured: with none set there is no
    // cookie and nothing to forge.
    if request.method() == Method::POST && is_cross_site(&request) {
        return (StatusCode::FORBIDDEN, "cross-site request rejected\n").into_response();
    }

    next.run(request).await
}

/// Whether this request belongs to the JSON API.
///
/// Path-based rather than `Accept`-based on purpose: `EventSource` sends
/// `Accept: text/event-stream` and cannot be made to send anything else, so
/// negotiating on the header would send an event stream to the login page.
fn is_api(request: &Request) -> bool {
    request.uri().path().starts_with("/api/")
}

fn wants_html(request: &Request) -> bool {
    request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"))
}

/// `Sec-Fetch-Site` is sent by every modern browser and settles the question
/// directly; `Origin` is the fallback for the rare client that omits it. A
/// request with neither (a script using the bearer path, or a test harness
/// with no browser fetch metadata) is not a browser request this control
/// exists for, so it is allowed through.
fn is_cross_site(request: &Request) -> bool {
    if let Some(site) = request
        .headers()
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
    {
        return site != "same-origin" && site != "none";
    }

    let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(host) = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return true;
    };
    origin.rsplit("://").next() != Some(host)
}

#[cfg(test)]
mod tests {
    /// The whole point of `SecretSource`: nothing under `src/web/` may ever
    /// call `Secret::expose`, or a settings page (or any other web code) can
    /// print a secret. Blunt on purpose — it is the one thing here that must
    /// never regress silently.
    #[test]
    fn nothing_under_src_web_calls_expose() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web");
        let mut offenders = Vec::new();
        let mut scanned = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_some_and(|ext| ext == "rs") {
                    let contents = std::fs::read_to_string(&path).expect("read source file");
                    // Built from parts so this very assertion does not trip
                    // the check against its own source file.
                    let needle = [".", "expose", "("].concat();
                    if contents.contains(&needle) {
                        offenders.push(path.clone());
                    }
                    scanned.push(path);
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "found a call to Secret::expose under src/web/: {offenders:?}"
        );

        // A walk over an empty or moved tree finds no offenders and passes,
        // which would make this guard decorative exactly when it matters most —
        // while the module is being split up or a file renamed. So require the
        // walk to have actually seen this file and a plausible number of others.
        // If the module genuinely shrinks, lower the floor deliberately.
        assert!(
            scanned.iter().any(|path| path.ends_with("mod.rs")),
            "the walk never reached src/web/mod.rs, so it proves nothing"
        );
        assert!(
            scanned.len() >= 6,
            "only {} source files scanned under src/web/",
            scanned.len()
        );
    }
}
