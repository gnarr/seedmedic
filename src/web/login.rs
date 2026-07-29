//! `GET`/`POST /login` and `POST /logout` — the browser-usable side of
//! `server.auth_token`. See docs/todos/0018-browser-usable-authentication.md.
//!
//! A browser cannot attach an `Authorization` header from an HTML form, so
//! the token needs somewhere else to live once it protects the UI: a session
//! cookie, minted here once the submitted value verifies against the
//! configured token. The cookie carries a random session id, never the token
//! itself — see `RuntimeHandle::create_session`.

use axum::{
    Form,
    extract::State,
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Redirect, Response},
};
use maud::html;
use serde::Deserialize;

use super::{AppState, layout};

pub(super) const COOKIE_NAME: &str = "seedmedic_session";

#[derive(Deserialize)]
pub struct LoginForm {
    token: String,
}

pub async fn show(State(state): State<AppState>) -> Response {
    render(state.runtime.current().auth_token.is_some(), None)
}

pub async fn submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let runtime = state.runtime.current();
    match &runtime.auth_token {
        None => render(false, None),
        Some(expected) if expected.verify(&form.token) => {
            let session_id = state.runtime.create_session();
            let mut response = Redirect::to("/").into_response();
            response
                .headers_mut()
                .insert(header::SET_COOKIE, cookie_header(&session_id, &headers));
            response
        }
        Some(_) => render(true, Some("Incorrect token.")),
    }
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(session_id) = session_id_from(&headers) {
        state.runtime.destroy_session(&session_id);
    }
    let mut response = Redirect::to("/login").into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_static(EXPIRED_COOKIE));
    response
}

const EXPIRED_COOKIE: &str = "seedmedic_session=; Path=/; Max-Age=0";

fn render(auth_enabled: bool, message: Option<&str>) -> Response {
    let body = html! {
        h1 { "Sign in" }
        @if let Some(message) = message {
            div.notice.danger { p { (message) } }
        }
        @if auth_enabled {
            form method="post" action="/login" {
                label {
                    "Token"
                    br;
                    input type="password" name="token" autofocus;
                }
                div.actions { button type="submit" { "Sign in" } }
            }
        } @else {
            p { "No auth token is configured — there is nothing to sign into." }
        }
    };
    layout::bare_page("Sign in", body).into_response()
}

/// `Set-Cookie` for a freshly minted session — `HttpOnly` and
/// `SameSite=Strict` unconditionally (the CSRF control this middleware
/// relies on, see `web::require_auth_token`), plus `Secure` whenever the
/// request arrived over HTTPS. This process never terminates TLS itself — see
/// the README — so `X-Forwarded-Proto` from a reverse proxy is the only
/// signal for that.
pub(super) fn cookie_header(session_id: &str, headers: &HeaderMap) -> HeaderValue {
    let mut value = format!("{COOKIE_NAME}={session_id}; HttpOnly; SameSite=Strict; Path=/");
    if is_https(headers) {
        value.push_str("; Secure");
    }
    HeaderValue::from_str(&value).expect("session id is hex, and the rest is a fixed ascii literal")
}

fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        == Some("https")
}

/// The live session id carried by the request's `Cookie` header, if any —
/// shared by the auth middleware and `POST /logout`.
pub(super) fn session_id_from(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == COOKIE_NAME).then(|| value.to_owned())
    })
}
