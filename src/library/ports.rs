use async_trait::async_trait;
use thiserror::Error;

use crate::not_implemented::NotImplemented;

use super::domain::{Candidate, CandidateQuery};

#[derive(Clone, Debug, Error)]
pub enum CandidateError {
    #[error(transparent)]
    NotImplemented(#[from] NotImplemented),
    #[error("candidate lookup failed: {0}")]
    Transport(String),
    #[error("candidate source returned data we cannot interpret: {0}")]
    Protocol(String),
    #[error("failed to read the media library: {0}")]
    Io(String),
}

impl CandidateError {
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::Io(_))
    }
}

/// Somewhere that can suggest library files for a torrent.
///
/// One method, because the workflow asks exactly one question. Sources are
/// additive: the repair queries every configured source and matches over the
/// union, so an *arr being down degrades quality rather than breaking discovery.
#[async_trait]
pub trait CandidateSource: Send + Sync {
    /// Short label for logs and the audit trail, e.g. `sonarr:main`.
    fn label(&self) -> &str;

    async fn find_candidates(
        &self,
        query: &CandidateQuery<'_>,
    ) -> Result<Vec<Candidate>, CandidateError>;
}
