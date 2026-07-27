# AGENTS.md — `src/tracker`

Supplements the root `AGENTS.md`.

## The tracker is the source of truth

Two facts only the tracker can establish:

- A hit-and-run exists.
- A hit-and-run has been cleared.

Nothing else may be used to infer either. A torrent seeding happily in
qBittorrent, a ratio that looks fine, a seed timer that has elapsed by our own
arithmetic — none of these complete a repair. `Seeding → Completed` happens when
`hit_and_run_status` returns `Cleared`, and at no other time.

`HitAndRunStatus::Unknown` exists so an adapter never has to choose between
lying and erroring. If the response parsed but did not mean anything we
recognise, return `Unknown`; the workflow keeps seeding and keeps asking.

## Adding an adapter

1. New file under `src/tracker/adapters/`, implementing `TrackerClient`.
2. Add a variant to `config::TrackerKind` and a match arm in
   `bootstrap::build_trackers`.
3. Do **not** add methods to the port. Three methods cover what a repair needs.
   If a tracker family offers something more, keep it inside the adapter until a
   use case in `repair/application/` actually asks for it.

## What an adapter owes the workflow

**Never invent a warning, never invent a clearance.** If the listing endpoint
returns something unexpected, return `TrackerError::Protocol`, not an empty
`Vec`. An empty list means "the tracker says there are none" and the workflow
believes it.

**Classify errors honestly.** `is_transient()` decides whether the workflow
retries with backoff or parks the job. Network trouble and rate limiting are
transient; a 401 and a schema that no longer parses are not — they need a human.

**Respect rate limits.** Private trackers ban for hammering. Handle `429` and
`Retry-After` by returning `RateLimited { retry_after_seconds }`; the worker
turns that into backoff. Never retry in a tight loop inside the adapter.

**Do not parse the `.torrent`.** `fetch_torrent_file` returns bytes. Decoding is
`torrent::TorrentInspector`'s job, and the bytes are what gets persisted.

**Treat every string as hostile.** Torrent names and paths from a tracker are
attacker-influenced. They reach the filesystem only through
`torrent::SafeRelativePath`; do not build a `PathBuf` from tracker data anywhere
in this module.

## Identifiers

`TrackerId` is operator-assigned in config and is the key repair jobs are filed
under — changing one orphans existing jobs. `TrackerTorrentId` is whatever the
tracker calls the torrent and is opaque: a Unit3D numeric id, a hash, a slug.
Nothing outside the adapter may assume a shape.

## Stubs

`unit3d.rs` currently fails every call with `NotImplemented`, which parks jobs
for review rather than reporting zero warnings. Keep that property in any new
stub: an unconfigured or unfinished tracker must be visible, not quiet. See
`docs/todos/0002-unit3d-tracker.md`.
