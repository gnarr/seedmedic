# 0008 — Recheck monitoring and safe-resume enforcement

**Status:** Not started
**Depends on:** 0006, 0007
**Blocks:** 0009, 0010, 0013

## Problem

The recheck and resume steps work against the fake client, which finishes a
check in a configurable number of polls and reports a single completeness ratio.
Real qBittorrent rechecking a 60 GB torrent behaves differently in ways the
current code does not handle:

- A check can take tens of minutes. The current fixed `recheck_poll_interval`
  polls at the same rate throughout and offers the operator no sense of progress.
- A check can be *queued* rather than running, which looks the same from
  `status` but means the wait will be much longer.
- A partial result today produces one number. What an operator actually needs to
  know is *which files* are missing — the difference between "one episode of a
  season pack is a different encode" and "nothing matched" is the difference
  between a five-minute fix and abandoning the repair.
- Nothing detects a check that never finishes, or a torrent that goes to
  `Errored` mid-check.

The resume gate itself is correct and well tested. This document is about making
the information going into it good enough to act on.

## Architectural context

`repair/application/verify.rs` holds both steps. `assess_data` decides whether
data is sound; `decide_resume` adds policy. `src/repair/AGENTS.md` is explicit
that policy can only ever make the answer more conservative — that stays true.

`StepOutcome::Wait` exists so polling does not spend the retry budget.

## Expected behaviour

- While a check is running, the job stays in `rechecking` and the UI shows
  progress.
- A check that has not finished within a configured ceiling parks the job for
  review rather than polling forever.
- A torrent that enters `Errored` during a check parks immediately, with the
  client's error surfaced.
- A partial result records per-file completeness, so review can say "S01E04 is
  the only mismatch".
- Everything the resume gate already refuses, it still refuses. The hardlink rule
  is untouched.

## Implementation steps

1. **Per-file completeness.** `GET /api/v2/torrents/files?hash=` returns each
   file's progress. Extend `seeding::TorrentStatus` with an optional
   `files: Vec<FileProgress>` — optional because not every client will offer it,
   and the resume gate must work without it. Persist the per-file result onto
   `repair_job_files` (a new nullable column, new migration) so the UI and the
   audit trail have it.

2. **Distinguish queued from running.** If the client can tell them apart, add a
   `Queued` variant or a flag, and use a longer poll interval for it. If not, say
   so here and move on.

3. **Adaptive polling.** A fixed 15-second poll on a 40-minute check is 160
   pointless requests. Back off: poll quickly at first, then progressively less
   often, capped. This is `Wait { after }` with a computed delay, so the state
   machine does not change — only the number.

4. **A ceiling.** Add `policy.recheck_timeout_seconds`. Track when the check
   started (the `injected → rechecking` transition timestamp is already in the
   audit trail; read it, or add a column). Past the ceiling, park with a new
   `ReviewReason::RecheckTimedOut`.

5. **Handle `Errored`.** `ClientTorrentState::Errored` during verification parks
   the job with the client's message in the audit detail. Do not retry — a
   torrent in an error state does not recover by being asked again.

6. **Surface progress in the UI.** The job detail page should show the check's
   progress and, on a partial result, which files matched. This is the
   information review is missing.

7. **Re-verify before resuming — keep it.** `resume` already re-reads status
   immediately before acting rather than trusting the verify step. That is
   deliberate; do not "optimise" it away.

## Invariants and safety constraints

- `assess_data` is unchanged. Incomplete plus aliased is never resumed, whatever
  the per-file detail says.
- Per-file data is *additional* evidence. Its absence must never make the gate
  more permissive.
- A timeout parks; it never resumes and never fails.
- Polling never consumes the retry budget. Only genuine errors do.
- Nothing here may resume a torrent the client reports as `Errored`.

## Likely files

- `src/repair/application/verify.rs`
- `src/seeding/domain.rs`, `src/seeding/ports.rs`
- `src/seeding/adapters/qbittorrent.rs`, `src/seeding/adapters/fake.rs`
- `src/repair/domain.rs` (`ReviewReason`)
- `src/repair/policy.rs`, `src/config.rs`
- `migrations/000N_file_progress.sql`
- `src/web/jobs.rs`

## Required tests

- A check reported as running keeps the job in `rechecking` without spending an
  attempt.
- Poll intervals back off across successive waits.
- A check exceeding the ceiling parks with `RecheckTimedOut`.
- `Errored` during a check parks immediately with the message recorded.
- A partial result with three of four files complete records exactly that, and
  the review page names the missing file.
- Per-file data absent: the resume gate behaves exactly as it does today
  (existing tests must pass unchanged).
- The hardlink-plus-incomplete case still refuses, with per-file data present
  and absent.

## Acceptance criteria

- A real recheck of a large torrent completes without a storm of requests.
- A stuck check is visible and parked, not silently pending forever.
- A review page for a partial match tells the operator which file is wrong.
- Every existing safety test passes unchanged.

## Out of scope

- Fixing a partial match by re-matching only the failed files. That is a good
  idea and belongs in 0010, where the operator can approve it.
- Letting the client download missing pieces from the swarm. That is the one
  thing SeedMedic must never do without an explicit, separate decision — if it is
  ever wanted, it needs its own document and its own policy gate.
- Throttling rechecks across concurrent jobs.

## Open questions

- What is a reasonable default ceiling? A 100 GB torrent on spinning rust can
  legitimately take an hour or more.
- Does the backoff need a floor for small torrents, where a 15-second first poll
  is already slower than the check?
- Should the per-file column live on `repair_job_files` or in the transition
  detail JSON? The column is queryable and the JSON is free; the UI probably
  wants the column.
- If exactly one file of a season pack fails, is parking the whole job right, or
  should the repair proceed with a torrent it knows is incomplete? It must not
  resume — but staging the rest and telling the operator "one file short" may be
  more useful than stopping at the recheck.
