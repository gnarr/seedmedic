//! `Matched → Staged`: build the torrent's layout in the staging area.
//!
//! This is the step that touches the filesystem, so it is the step with the
//! most ways to refuse. It re-checks every source file's size immediately
//! before using it (the library may have changed since matching), and it
//! records which strategy each file actually got — the resume policy later
//! depends on knowing whether anything is hardlinked.

use serde_json::json;

use crate::{
    repair::{
        application::StepOutcome,
        domain::{RepairJob, ReviewReason},
        ports::JobPatch,
        worker::RepairDeps,
    },
    staging::{MaterializationPlan, PlanItem, StagingError},
};

pub async fn stage_files(deps: &RepairDeps, job: &RepairJob) -> StepOutcome {
    let staging_dir = job
        .staging_dir
        .clone()
        .unwrap_or_else(|| job.default_staging_dir());

    let planned = match deps.store.planned_files(job.id).await {
        Ok(files) => files,
        Err(error) => return StepOutcome::retry(error),
    };

    let mut items = Vec::with_capacity(planned.len());
    for file in &planned {
        let Some(source) = file.source.clone() else {
            // Matching promised every file a source. If one is missing the
            // plan is not what we think it is; a human should look.
            return StepOutcome::review(
                ReviewReason::AmbiguousMatch,
                json!({ "error": "file has no matched source", "path": file.torrent_path.as_str() }),
            );
        };
        items.push(PlanItem {
            source,
            destination: file.torrent_path.under(&staging_dir),
            length: file.length,
        });
    }

    let preference = deps.policy.materialization.preference();
    if preference.is_empty() {
        return StepOutcome::review(
            ReviewReason::MaterializationUnavailable,
            json!({ "error": "configuration permits no materialization strategy" }),
        );
    }

    let plan = MaterializationPlan { items };
    let layout = match deps.staging.materialize(&plan, &preference).await {
        Ok(layout) => layout,
        Err(StagingError::NotImplemented(details)) => {
            return StepOutcome::review(
                ReviewReason::AdapterNotImplemented,
                json!({ "adapter": details.adapter, "todo": details.todo }),
            );
        }
        Err(
            error @ (StagingError::StrategyUnavailable { .. } | StagingError::NoStrategyPermitted),
        ) => {
            return StepOutcome::review(
                ReviewReason::MaterializationUnavailable,
                json!({ "error": error.to_string(), "tried": format!("{preference:?}") }),
            );
        }
        Err(error @ (StagingError::SourceMissing(_) | StagingError::SourceChanged { .. })) => {
            return StepOutcome::review(
                ReviewReason::LibraryChanged,
                json!({ "error": error.to_string() }),
            );
        }
        Err(error @ StagingError::UnsafePath { .. }) => {
            return StepOutcome::review(
                ReviewReason::UnsafeTorrentPaths,
                json!({ "error": error.to_string() }),
            );
        }
        Err(error) => return StepOutcome::retry(error),
    };

    let staged: std::collections::HashMap<_, _> = layout
        .files
        .iter()
        .map(|file| (file.path.clone(), file.strategy))
        .collect();

    let files = planned
        .into_iter()
        .map(|mut file| {
            file.materialized_as = staged.get(&file.torrent_path.under(&staging_dir)).copied();
            file
        })
        .collect();

    StepOutcome::advance_with(
        json!({
            "staging_dir": staging_dir.as_str(),
            "strategy": layout.summary_strategy().map(|strategy| strategy.as_str()),
            "aliases_library": layout.aliases_library_files(),
            "bytes": layout.total_bytes(),
        }),
        JobPatch {
            staging_dir: Some(staging_dir),
            materialization: layout.summary_strategy(),
            files: Some(files),
            ..JobPatch::default()
        },
    )
}
