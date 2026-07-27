# 0009 — Tracker-side completion and seed monitoring

**Status:** Done
**Depends on:** 0002, 0008
**Blocks:** 0013

## Problem

`confirm_with_tracker` polls the tracker until the hit-and-run clears. That is
the right shape, but a repair sits in `seeding` for hours or days, and during
that window the current implementation is blind to everything except the
tracker's answer:

- Nothing checks that the torrent is *still seeding*. If qBittorrent errors,
  pauses, or loses the data, the job waits for a clearance that will never come.
- `HitAndRunStatus::Unknown` waits forever without escalating. A tracker that
  changed its API silently stalls every repair.
- There is no sense of progress. The operator cannot tell whether a repair is
  two hours from clearing or has been stuck for a week.
- The tracker is polled on a fixed interval regardless of how far off the
  deadline is.

## Architectural context

`src/tracker/AGENTS.md`: the tracker is the only thing that can say a
hit-and-run is cleared, and `Unknown` exists so an adapter never has to choose
between lying and erroring. Both stay true. This document adds the *other*
checks that run alongside the tracker poll.

`StepOutcome::Wait` does not consume the retry budget, which is what makes a
multi-day wait viable.

## Expected behaviour

- While waiting for the tracker, the client is also checked: a torrent that
  stopped seeding is noticed and dealt with, not waited on.
- Repeated `Unknown` answers escalate to review rather than continuing forever.
- The job records seeding progress — uploaded bytes, elapsed seed time — so the
  UI can show how a repair is doing.
- Polling adapts: more often as a deadline approaches, less often for a repair
  with days to run.
- A hit-and-run whose deadline has passed without clearing is escalated, since
  that means the repair did not work.

## Implementation steps

1. **Check the client alongside the tracker.** In `confirm_with_tracker`, read
   `status` as well. Handle:
   - `Seeding` — normal, keep waiting.
   - `Paused` — somebody paused it, or a restart did. Attempt one resume through
     the same `decide_resume` gate that governed the original one; if the gate
     refuses, park. Never resume without re-asking.
   - `Downloading` — the torrent is *fetching* data, which means the staged data
     was not complete after all. Park immediately with a distinct reason; this is
     the case that most needs a human.
   - `Errored`, `None` — rewind (`StepOutcome::Rewind`) to `staged`, as the
     verification steps already do.

2. **Escalate persistent `Unknown`.** Count consecutive `Unknown` answers on the
   job (a column, or derived from the audit trail) and park with
   `TrackerStatusUnclear` past a threshold. One `Unknown` is a blip; twenty is a
   broken adapter.

3. **Deadline awareness.** `HitAndRun.deadline` is already modelled but not
   persisted. Add a column, populate it at discovery, and use it: poll more
   frequently near the deadline, and park with a new reason when it passes
   without clearing.

4. **Seeding progress.** Record uploaded bytes and, if the tracker exposes it,
   elapsed and required seed time. Persist enough that the job detail page can
   show "3 of 72 hours". Where this comes from — client, tracker, or both — is an
   open question below.

5. **Adaptive tracker polling.** The default 15-minute interval is either too
   often for a three-day requirement or too rare near a deadline. Derive it from
   the time remaining, with a floor that respects the tracker's rate limits.

6. **Completion tidy-up.** On reaching `completed`, decide what happens to the
   staged data. It is still seeding, so it cannot be deleted — but the job is
   done, and the operator needs to know the staging directory is now permanent.
   Say so in the UI at minimum; retention policy belongs in 0010.

## Invariants and safety constraints

- Only the tracker completes a repair. No amount of seed time, uploaded bytes,
  or client state may substitute for `Cleared`.
- `Unknown` is never treated as `Cleared`. Escalating to review is the only
  alternative to waiting.
- A resume during the seeding phase goes through `decide_resume`, exactly like
  the original.
- `Downloading` during seeding is a safety event: the client is fetching data
  into the staging directory, and if anything is hardlinked, into the library.
  Park immediately and loudly.
- Tracker polling respects rate limits. A repair waiting three days must not
  generate three days of traffic.

## Likely files

- `src/repair/application/confirm.rs`
- `src/repair/domain.rs` (`ReviewReason`, deadline field)
- `src/tracker/domain.rs` (seed-time fields, if the API offers them)
- `src/repair/policy.rs`, `src/config.rs`
- `migrations/000N_seeding_progress.sql`
- `src/web/jobs.rs`

## Required tests

- `Active` plus a seeding client waits without spending an attempt.
- `Cleared` completes.
- Twenty consecutive `Unknown` answers park with `TrackerStatusUnclear`.
- A paused torrent is resumed through the gate; with `auto_resume = never` it
  parks instead.
- A `Downloading` torrent parks immediately.
- A torrent missing from the client rewinds to `staged`.
- A deadline in the past with the warning still active parks.
- Poll intervals shorten as the deadline approaches and respect the floor.
- Seeding progress is persisted and rendered.

## Acceptance criteria

- A real repair clears on a real tracker and reaches `completed`.
- A repair whose torrent is paused by hand is noticed within one poll.
- A week-long seed generates a modest, bounded number of tracker requests.
- The job page shows how far along a seed is.

## Out of scope

- Enforcing seed time ourselves. The tracker's arithmetic is the one that counts.
- Cross-seeding the repaired data to other trackers.
- Deleting or archiving staged data — 0010.

## Open questions

- Where does seed time come from? qBittorrent tracks `seeding_time` per torrent,
  but the tracker's count is the one that matters and the two will disagree.
  Showing both, labelled, may be the honest answer.

  **Resolved:** the tracker port exposes exactly three methods
  (`src/tracker/AGENTS.md`), none of which return a seed-time figure, and
  neither adapter's API offers a "required hours remaining" value tied to a
  specific torrent. Inventing a fourth port method for a number no adapter can
  actually supply would be a stub with extra steps. This document's seeding
  progress is therefore client-only: uploaded bytes and elapsed seeding time,
  both from `TorrentClient::status`, labelled as the client's own accounting.
  If a tracker family later exposes a per-torrent seed-time figure, add it then,
  as its own field, rather than guessing at one now.
- How many `Unknown` answers before escalating? It depends on the poll interval;
  express the threshold in time rather than count?

  **Resolved:** count, not time. The poll interval already adapts to the
  deadline (this document, "Adaptive tracker polling"), so a fixed count scales
  with it automatically — more `Unknown` answers fit in the same wall-clock
  time when polling is frequent, which is exactly when a broken adapter should
  be caught fastest. `policy.max_consecutive_unknown_tracker_status` defaults
  to 20.
- When a deadline passes with the warning outstanding, is the repair `failed` or
  `awaiting_review`?

  **Resolved:** `awaiting_review`, via a new `ReviewReason::HitAndRunDeadlinePassed`.
  Every other automated stop in this lifecycle parks rather than fails —
  `failed` is reserved for an operator's own decision (`OperatorAbandon`) — and
  the repair may still be seeding usefully even after the deadline it was
  tracking has passed.
- Should a completed repair keep being monitored in case the tracker re-flags it?
  That would mean `completed` is not terminal, which is a bigger change than it
  looks.

  **Resolved:** out of scope here. `completed` stays terminal. A tracker
  re-opening a cleared hit-and-run is not a case any adapter currently
  produces, and building for it now would be speculative.
