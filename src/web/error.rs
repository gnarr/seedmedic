use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use maud::html;
use tracing::error;

use crate::repair::StoreError;

use super::layout;

#[derive(Debug)]
pub enum WebError {
    NotFound,
    /// The operator asked for something the state machine will not allow —
    /// usually because the job moved since the page was rendered.
    Refused(String),
    Store(StoreError),
}

impl From<StoreError> for WebError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "No such repair job.".to_owned()),
            Self::Refused(message) => (StatusCode::CONFLICT, message),
            Self::Store(error) => {
                error!(%error, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Something went wrong reading repair state.".to_owned(),
                )
            }
        };

        let body = layout::page(
            "Error",
            html! {
                div.notice.danger { p { (message) } }
                p { a href="/" { "Back to repairs" } }
            },
        );

        (status, body).into_response()
    }
}
