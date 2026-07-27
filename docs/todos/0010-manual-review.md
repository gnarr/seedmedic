# 0010 — Manual-review workflows

**Status:** Not started
**Depends on:** 0005, 0008
**Blocks:** nothing

## Problem

Review is where SeedMedic sends everything it will not decide alone, so the
quality of the review experience determines whether the conservative defaults
are usable or merely annoying. Right now an operator gets three buttons — retry,
start over, abandon — and no way to actually resolve anything.

The gaps, in rough order of how often they will bite:

1. **Approving a resume is impossible.** A repair that reaches `verified` with
   the default `auto_resume = "never"` parks on `AutoResumeDisabled`, and
   "Retry" re-runs the same policy check and parks again. The only way forward is
   to edit the config file and restart. This makes the safe default effectively
   unusable, which is the worst possible outcome for a safety setting.

2. **An ambiguous match cannot be resolved.** The operator can see that three
   files were candidates but cannot say which one is right.

3. **A partial recheck cannot be acted on.** After 0008 the operator can see
   which file is wrong; they still cannot substitute a different one.

4. **Nothing is bulk-actionable.** Twenty jobs parked on the same
   `AdapterNotImplemented` need twenty clicks.

5. **Staging is never cleaned up.** Abandoned jobs leave their staged files on
   disk forever.

## Architectural context

The review actions live in `src/web/review.rs` and are deliberately thin: each
is one validated transition, recorded with an `operator_*` reason so the audit
trail shows a human did it. `validate_transition` enforces that a retry may only
resume `review_from_state` — review must not be a way to skip work.

Everything added here must keep both properties: operator actions are recorded
as transitions, and they cannot bypass a step.

## Expected behaviour

- **Approve resume.** On a job parked with `AutoResumeDisabled`, one button
  resumes it — this job only, recorded as an operator decision, without changing
  global policy.
- **Choose a candidate.** On a job parked with `AmbiguousMatch` or
  `ConfidenceBelowPolicy`, the operator sees the candidates that were considered
  with their evidence, and can pick one per file. The job then resumes at
  `matched` with the chosen plan.
- **Bulk actions.** Select several jobs on the list page and retry or abandon
  them together.
- **Cleanup.** Abandoning offers to discard staged data. Completed jobs report
  how much space they are holding.

## Implementation steps

1. **Per-job resume approval.** Add a `resume_approved` boolean column, set by
   an operator action, and have `decide_resume` accept it as an override of
   `AutoResume::Never` — **and only of that**. It must not override
   `assess_data`: approving a resume on incomplete hardlinked data stays
   impossible, and the button must not even be offered there. Add a policy test
   asserting an approved job with partial aliased data still refuses.

2. **Candidate override.** Persist the rejected candidates, not just the chosen
   one — probably as JSON in the `matched` transition's detail, which is already
   the audit record, rather than a new table. The review page renders them; the
   operator picks; the action writes the chosen `source_path` into
   `repair_job_files` and transitions back to `matched` with reason
   `operator_retry`. The staging step then proceeds with the operator's choice,
   and `MatchConfidence` for that file becomes something explicit —
   `MatchConfidence::Operator`, or `Exact` with evidence recording that a human
   chose it. Prefer a distinct variant; conflating a human's guess with a
   verified hash is the kind of thing that reads fine now and is confusing in a
   year.

3. **Bulk actions.** A form posting a list of job ids to a bulk endpoint,
   applying the same per-job validated transition to each and reporting per-job
   results. No new transition semantics — just a loop with a summary.

4. **Cleanup.** On abandon, offer "discard staged files" (the existing
   `StagingFilesystem::discard`, which is already narrow and safe). Show staged
   size on the job page. Consider a retention setting for completed jobs — but
   note that completed jobs are *still seeding*, so their data must not be
   deleted while the torrent is live. Deletion must remove the torrent from the
   client first, and that means the hit-and-run could come back. Think this
   through before building it.

5. **Make the review queue scannable.** Group the list by reason, so twenty jobs
   blocked on the same missing adapter read as one problem.

## Invariants and safety constraints

- **An operator may lower a policy gate, never a safety rule.** `assess_data`
  stays absolute: no button resumes incomplete data that aliases the library.
- Approval is per job and recorded. It never mutates global policy.
- A candidate override still goes through staging and a full recheck. The
  operator chooses the file; qBittorrent still verifies it.
- Every operator action is one validated transition with an `operator_*` reason.
- A retry still resumes exactly `review_from_state`.
- Deleting staged data requires the torrent to be gone from the client first,
  and never passes `delete_files` to the client.

## Likely files

- `src/web/review.rs`, `src/web/jobs.rs`
- `src/repair/policy.rs` (approval override)
- `src/repair/domain.rs`, `src/repair/ports.rs`
- `src/library/domain.rs` (`MatchConfidence` variant)
- `migrations/000N_review_approval.sql`

## Required tests

- Approving a resume on complete data resumes it; global policy is unchanged and
  other jobs still park.
- Approving a resume on incomplete aliased data is refused, and the button is
  not rendered.
- Choosing a candidate rewrites the file plan and resumes at `matched`.
- An overridden match still goes through staging and recheck.
- A bulk retry over five jobs, one of which moved in the meantime, applies four
  and reports the conflict.
- Abandon-with-discard removes the staging directory and nothing else.
- Every operator action appears in the audit trail with an `operator_*` reason.

## Acceptance criteria

- `auto_resume = "never"` is a comfortable default: parked repairs can be
  approved from the UI in one click.
- An ambiguous match is resolvable without touching the database.
- No operator action can reach a state the state machine would refuse.

## Out of scope

- Authentication. The UI is unauthenticated and assumed to be on a trusted
  network — see 0011, which should at least document that clearly.
- Notifications. Worth having; belongs with 0012.
- Editing torrent paths or the file plan beyond choosing among discovered
  candidates.

## Open questions

- Is `resume_approved` a column or a transition? A column is simpler; a
  transition means approval is in the audit trail by construction. Possibly
  both — the column derived from the transition.
- Should approval expire? A job approved and then rewound by reconciliation
  probably should not stay approved through a re-stage with different data.
  Clearing approval on any rewind is the conservative choice.
- New `MatchConfidence::Operator` variant, or `Exact` with an evidence flag? A
  variant changes the `Ord` used by the policy floor, which needs care.
- Is a bulk "approve resume" acceptable, or should the one genuinely dangerous
  action stay one at a time?
