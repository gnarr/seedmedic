//! Media candidate discovery and matching.
//!
//! Everything here treats the user's library as read-only. Nothing in this
//! module opens a file for writing, and nothing outside `staging` may act on a
//! [`Candidate`] path.

pub mod adapters;
mod domain;
pub mod matching;
mod ports;
pub mod verification;

pub use domain::{
    Candidate, CandidateOrigin, CandidateQuery, FileMatch, MatchConfidence, MatchEvidence,
    MatchPlan, UnmatchedFile, UnmatchedReason,
};
pub use matching::plan_matches;
pub use ports::{CandidateError, CandidateSource};
