use async_trait::async_trait;

use crate::library::{
    domain::{Candidate, CandidateQuery},
    ports::{CandidateError, CandidateSource},
};

/// A fixed set of candidates, returned for every query.
pub struct FakeCandidateSource {
    label: String,
    candidates: Vec<Candidate>,
}

impl FakeCandidateSource {
    pub fn new(label: impl Into<String>, candidates: Vec<Candidate>) -> Self {
        Self {
            label: label.into(),
            candidates,
        }
    }
}

#[async_trait]
impl CandidateSource for FakeCandidateSource {
    fn label(&self) -> &str {
        &self.label
    }

    async fn find_candidates(
        &self,
        _query: &CandidateQuery<'_>,
    ) -> Result<Vec<Candidate>, CandidateError> {
        Ok(self.candidates.clone())
    }
}
