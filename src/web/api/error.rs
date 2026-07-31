//! One error shape for every endpoint.
//!
//! ```json
//! { "error": { "code": "invalid_transition",
//!              "message": "cannot move a repair from seeding to failed …",
//!              "fields": {"policy.max_attempts": "must be at least 1"},
//!              "general": [], "ignored": [] } }
//! ```
//!
//! `message` is the existing `Display` text, verbatim. The operator-facing prose
//! in `InvalidTransition`, `ReloadError::Refused` and `Config::problems()` is
//! better than anything a client could compose, and rewriting it here would put
//! a second wording in a second place.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::collections::BTreeMap;

use crate::{
    repair::{InvalidTransition, StoreError},
    runtime::ReloadError,
    web::error::WebError,
};

/// Per-field validation messages, keyed by the concrete dotted TOML key —
/// `policy.max_attempts`, `trackers.0.base_url`. The same type
/// `web::settings::save` already produces, carried straight to the wire so the
/// client can put each message under the input it belongs to.
pub type FieldErrors = BTreeMap<String, String>;

#[derive(Debug)]
pub enum ApiError {
    /// No such job, or no such settings page.
    NotFound(&'static str),
    /// The request was well-formed but the resource's state conflicts. 409 —
    /// the SPA's remedy is refetch and re-render, which is exactly 409's
    /// canonical meaning.
    Conflict(String),
    /// The operator typed something invalid. 422, distinct from 400, so the
    /// client can tell "my JSON is wrong" (a bug — show a generic error) from
    /// "paint this field red" without parsing `code`.
    Invalid {
        message: String,
        fields: FieldErrors,
        general: Vec<String>,
    },
    /// A submitted key is not in `FIELDS`. `deny_unknown_fields` at the HTTP
    /// layer; 400, because a correct client cannot produce it.
    UnknownField(String),
    /// Something went wrong that is not the caller's fault. The real error is
    /// logged and never returned — it can name paths and internals.
    Internal(&'static str),
}

impl ApiError {
    fn parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            Self::NotFound(message) => (StatusCode::NOT_FOUND, "not_found", (*message).to_owned()),
            Self::Conflict(message) => (StatusCode::CONFLICT, "refused", message.clone()),
            Self::Invalid { message, .. } => {
                (StatusCode::UNPROCESSABLE_ENTITY, "invalid", message.clone())
            }
            Self::UnknownField(key) => (
                StatusCode::BAD_REQUEST,
                "unknown_field",
                format!("`{key}` is not a setting SeedMedic has."),
            ),
            Self::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                (*message).to_owned(),
            ),
        }
    }

    pub fn invalid(message: impl Into<String>, fields: FieldErrors, general: Vec<String>) -> Self {
        Self::Invalid {
            message: message.into(),
            fields,
            general,
        }
    }
}

#[derive(Serialize)]
struct Body<'a> {
    error: Payload<'a>,
}

#[derive(Serialize)]
struct Payload<'a> {
    code: &'static str,
    message: String,
    /// Always present, even when empty, so a client never has to branch on
    /// whether the key exists before iterating it.
    fields: &'a FieldErrors,
    general: &'a [String],
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = self.parts();
        let empty_fields = FieldErrors::new();
        let empty_general: Vec<String> = Vec::new();
        let (fields, general) = match &self {
            Self::Invalid {
                fields, general, ..
            } => (fields, general),
            _ => (&empty_fields, &empty_general),
        };

        (
            status,
            Json(Body {
                error: Payload {
                    code,
                    message,
                    fields,
                    general: general.as_slice(),
                },
            }),
        )
            .into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Missing(_) => Self::NotFound("No such repair job."),
            // The job moved since the page was rendered. Same prose the maud UI
            // used, because it is the right prose.
            StoreError::Conflict { .. } | StoreError::Invalid(_) => Self::Conflict(format!(
                "{error} The job may have moved since this page was loaded."
            )),
            StoreError::Corrupt { .. } | StoreError::Database(_) => {
                tracing::error!(%error, "store error serving an API request");
                Self::Internal("Something went wrong reading repair state.")
            }
        }
    }
}

impl From<InvalidTransition> for ApiError {
    fn from(error: InvalidTransition) -> Self {
        Self::Conflict(format!(
            "{error} The job may have moved since this page was loaded."
        ))
    }
}

/// The maud layer's error type maps straight across, so the operator actions can
/// keep **one** implementation of their guards while both transports exist. Every
/// refusal message is already written for an operator; this changes the envelope,
/// never the words.
impl From<WebError> for ApiError {
    fn from(error: WebError) -> Self {
        match error {
            WebError::NotFound => Self::NotFound("No such repair job."),
            WebError::Refused(message) => Self::Conflict(message),
            WebError::Store(error) => Self::from(error),
        }
    }
}

impl From<ReloadError> for ApiError {
    fn from(error: ReloadError) -> Self {
        match error {
            // `check_refusals`' messages are written for an operator and say
            // what to do about it. 409, not 500: nothing is broken, the change
            // is refused.
            ReloadError::Refused(message) => Self::Conflict(message),
            ReloadError::Config(_) | ReloadError::Build(_) => {
                tracing::error!(%error, "reload failed serving an API request");
                Self::Internal("The configuration was saved but could not be applied.")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_conflict_keeps_the_it_may_have_moved_hint() {
        let error = ApiError::from(StoreError::Conflict {
            id: crate::repair::JobId(4),
            expected: crate::repair::RepairState::Staged,
            actual: crate::repair::RepairState::Failed,
        });
        let (status, code, message) = error.parts();

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(code, "refused");
        assert!(
            message.contains("may have moved"),
            "the hint is what tells an operator to reload rather than retry: {message}"
        );
    }

    /// A database error must never put its own text on the wire: it can name
    /// paths, table names and connection strings.
    #[test]
    fn a_database_error_is_not_echoed_to_the_caller() {
        let error = ApiError::from(StoreError::Database(
            "no such file: /srv/secret/path.db".to_owned(),
        ));
        let (status, _, message) = error.parts();

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!message.contains("/srv/secret"), "{message}");
    }

    #[test]
    fn validation_is_422_so_a_client_can_tell_it_from_its_own_bug() {
        let error = ApiError::invalid("1 setting could not be saved.", FieldErrors::new(), vec![]);
        assert_eq!(error.parts().0, StatusCode::UNPROCESSABLE_ENTITY);

        let error = ApiError::UnknownField("policy.nonsense".to_owned());
        assert_eq!(error.parts().0, StatusCode::BAD_REQUEST);
    }
}
