//! Sonarr and Radarr candidate discovery. Not implemented in the bootstrap.
//!
//! See `docs/todos/0004-arr-candidate-discovery.md` for the lookup strategy
//! (release name → series/movie → episode files → paths) and for why both
//! services share one adapter shape.

use async_trait::async_trait;
use url::Url;

use crate::{
    library::{
        domain::{Candidate, CandidateQuery},
        ports::{CandidateError, CandidateSource},
    },
    not_implemented::NotImplemented,
};

const TODO: &str = "docs/todos/0004-arr-candidate-discovery.md";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArrKind {
    Sonarr,
    Radarr,
}

impl ArrKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sonarr => "sonarr",
            Self::Radarr => "radarr",
        }
    }
}

pub struct ArrCandidateSource {
    label: String,
    #[allow(dead_code, reason = "used once docs/todos/0004 lands")]
    kind: ArrKind,
    #[allow(dead_code, reason = "used once docs/todos/0004 lands")]
    base_url: Url,
}

impl ArrCandidateSource {
    pub fn new(kind: ArrKind, instance: &str, base_url: Url) -> Self {
        Self {
            label: format!("{}:{instance}", kind.as_str()),
            kind,
            base_url,
        }
    }
}

#[async_trait]
impl CandidateSource for ArrCandidateSource {
    fn label(&self) -> &str {
        &self.label
    }

    async fn find_candidates(
        &self,
        _query: &CandidateQuery<'_>,
    ) -> Result<Vec<Candidate>, CandidateError> {
        Err(NotImplemented::new("library::adapters::arr", TODO).into())
    }
}
