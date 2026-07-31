//! The session as one resource: `GET`, `POST`, `DELETE /api/v1/session`.
//!
//! Collapsing `/login` and `/logout` into one resource is not cosmetic — it is
//! what lets the auth middleware's exempt list shrink to a single path, and
//! eventually to nothing, because exemption becomes structural (an unguarded
//! router merged in) rather than a `matches!` somebody has to remember to
//! update.
//!
//! `GET` is exempt from authentication. It reveals only whether a token is
//! configured, which is exactly what the maud UI's "No auth token is set"
//! banner already told anyone who could reach the port — and the SPA cannot
//! know whether to show a login screen without it.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::web::{AppState, login};

#[derive(Serialize)]
pub struct SessionView {
    /// Three states, not two. See `Chrome::auth`.
    auth: &'static str,
    /// Whether this request *would* pass the middleware — including when no
    /// token is configured at all, so the client's guard is one boolean rather
    /// than a combination it has to reason about.
    authenticated: bool,
    /// What the client needs before it can render anything: the shell is served
    /// unauthenticated, so this is the first call it makes.
    app: App,
}

#[derive(Serialize)]
struct App {
    version: &'static str,
    features: Features,
}

#[derive(Serialize)]
struct Features {
    /// Whether "Load demo configuration" can be offered at all.
    fakes: bool,
    metrics: bool,
}

#[derive(Deserialize)]
pub struct Credentials {
    token: String,
}

pub async fn show(State(state): State<AppState>, headers: HeaderMap) -> Json<SessionView> {
    let runtime = state.runtime.current();
    let authenticated = match &runtime.auth_token {
        // No token configured means the middleware is a no-op, so every request
        // is authenticated. Saying `false` here would send the SPA to a login
        // screen with nothing to type.
        None => true,
        Some(expected) => {
            let bearer = headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .is_some_and(|token| expected.verify(token));
            bearer
                || login::session_id_from(&headers).is_some_and(|id| state.runtime.has_session(&id))
        }
    };

    Json(SessionView {
        auth: runtime.chrome.auth(),
        authenticated,
        app: App {
            version: env!("CARGO_PKG_VERSION"),
            features: Features {
                fakes: cfg!(feature = "fakes"),
                metrics: cfg!(feature = "metrics"),
            },
        },
    })
}

/// Sign in. 204 plus the cookie, or 401 — and **never** the token in any header
/// or body, which `tests/web_auth.rs` asserts over the whole response.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(credentials): Json<Credentials>,
) -> Response {
    let runtime = state.runtime.current();
    match &runtime.auth_token {
        None => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": {
                    "code": "refused",
                    "message": "No auth token is configured, so there is nothing to sign in to.",
                    "fields": {}, "general": []
                }
            })),
        )
            .into_response(),
        Some(expected) if expected.verify(&credentials.token) => {
            let session_id = state.runtime.create_session();
            let mut response = StatusCode::NO_CONTENT.into_response();
            response.headers_mut().insert(
                header::SET_COOKIE,
                login::cookie_header(&session_id, &headers),
            );
            response
        }
        Some(_) => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": {
                    "code": "bad_credentials",
                    "message": "Incorrect token.",
                    "fields": {}, "general": []
                }
            })),
        )
            .into_response(),
    }
}

/// Sign out. Exempt from authentication on purpose: logging out when the session
/// has already been invalidated — by a token change, or a restart — must succeed
/// rather than 401, or the client is stuck holding a dead cookie it cannot clear.
pub async fn destroy(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(session_id) = login::session_id_from(&headers) {
        state.runtime.destroy_session(&session_id);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        axum::http::HeaderValue::from_static(login::EXPIRED_COOKIE),
    );
    response
}
