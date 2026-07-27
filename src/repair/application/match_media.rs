//! `TorrentFetched → Matched`: choose a library file for every torrent file.
//!
//! Queries every configured candidate source and matches over the union. A
//! source that fails degrades the result rather than failing the step — but if
//! *nothing* answered, the step retries instead of concluding "no candidates",
//! because those two are not the same thing.

use serde_json::json;
use tracing::warn;

use crate::{
    library::{Candidate, CandidateError, CandidateQuery, plan_matches},
    repair::{
        application::StepOutcome,
        domain::{RepairJob, ReviewReason},
        policy::{MatchDecision, decide_match},
        ports::{JobPatch, PlannedFile},
        worker::RepairDeps,
    },
    torrent::TorrentFile,
};

pub async fn match_media(deps: &RepairDeps, job: &RepairJob) -> StepOutcome {
    let planned = match deps.store.planned_files(job.id).await {
        Ok(files) if !files.is_empty() => files,
        Ok(_) => {
            return StepOutcome::review(
                ReviewReason::TorrentUnreadable,
                json!({ "error": "the torrent recorded no files" }),
            );
        }
        Err(error) => return StepOutcome::retry(error),
    };

    let files: Vec<TorrentFile> = planned
        .iter()
        .map(|file| TorrentFile {
            path: file.torrent_path.clone(),
            length: file.length,
        })
        .collect();

    let query = CandidateQuery {
        torrent_name: &job.torrent_name,
        files: &files,
    };

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut failures = Vec::new();
    let mut stubbed = false;

    for source in &deps.candidate_sources {
        match source.find_candidates(&query).await {
            Ok(found) => candidates.extend(found),
            Err(CandidateError::NotImplemented(details)) => {
                stubbed = true;
                failures.push(json!({ "source": source.label(), "todo": details.todo }));
            }
            Err(error) => {
                warn!(source = source.label(), %error, "candidate source failed");
                failures.push(json!({ "source": source.label(), "error": error.to_string() }));
            }
        }
    }

    // Nothing answered at all: "we could not look" must not be recorded as "we
    // looked and found nothing".
    if candidates.is_empty() && !failures.is_empty() {
        return if stubbed && failures.len() == deps.candidate_sources.len() {
            StepOutcome::review(
                ReviewReason::AdapterNotImplemented,
                json!({ "sources": failures }),
            )
        } else {
            StepOutcome::retry(format!("every candidate source failed: {failures:?}"))
        };
    }

    let plan = plan_matches(&files, &candidates);
    let detail = json!({
        "candidates": candidates.len(),
        "matched": plan.matched.len(),
        "unmatched": plan.unmatched.iter().map(|file| json!({
            "path": file.torrent_path.as_str(),
            "reason": file.reason,
        })).collect::<Vec<_>>(),
        "sources_failed": failures,
    });

    match decide_match(&plan, &deps.policy) {
        MatchDecision::HoldForReview(reason) => StepOutcome::review(reason, detail),
        MatchDecision::Accept => {
            let files = plan
                .matched
                .iter()
                .map(|matched| PlannedFile {
                    torrent_path: matched.torrent_path.clone(),
                    length: matched.length,
                    source: Some(matched.source.clone()),
                    confidence: Some(matched.confidence),
                    evidence: Some(matched.evidence),
                    materialized_as: None,
                })
                .collect();

            StepOutcome::advance_with(
                detail,
                JobPatch {
                    staging_dir: Some(job.default_staging_dir()),
                    files: Some(files),
                    ..JobPatch::default()
                },
            )
        }
    }
}
