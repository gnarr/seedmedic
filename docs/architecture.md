# SeedMedic architecture

A companion to the root `AGENTS.md`, which holds the rules. This document
explains the shape and the reasoning behind it.

## The problem

Private trackers penalise hit-and-runs: download a torrent, fail to seed it for
the required time, take a warning or a ban. The usual cause is mundane — the
client was reset, the data was moved by an *arr, the disk was swapped — and the
content is usually still sitting in the media library under a different name and
directory structure.

Repairing that by hand means finding the torrent, working out which library
files correspond to which torrent files, rebuilding the exact directory layout
somewhere the client can see it, and re-adding the torrent without letting the
client overwrite anything. SeedMedic automates that, conservatively.

## The pipeline

```
tracker ──1── .torrent ──2── library ──3── staging ──4── qBittorrent ──5── tracker
   discovery      inspection    matching    materialising    verify/seed   confirmation
```

1. **Discovery** — poll each tracker for outstanding hit-and-runs, upsert a
   repair job per warning.
2. **Inspection** — download the `.torrent`, decode the file list and the
   info-hash, validate every path.
3. **Matching** — ask each candidate source (Sonarr, Radarr, a filesystem walk)
   what it has, and pair every torrent file with a library file, with a
   confidence and the evidence behind it.
4. **Materialising** — recreate the torrent's directory layout under the staging
   root, using reflinks if possible, copies otherwise, hardlinks only if
   explicitly permitted.
5. **Verification and seeding** — add to qBittorrent paused, force a hash check,
   and resume only if the data is complete and policy allows.
6. **Confirmation** — keep asking the tracker until the warning is gone.

Anything ambiguous, incomplete, or unsafe at any point parks the job for a human
instead of guessing.

## Why a modular monolith

One process, one SQLite file, one binary. A self-hosted service repairing a
handful of torrents has no scaling problem, no multi-tenancy, and no team
boundary — the three things that justify splitting a system up. Queues, workers,
and services would add operational surface without removing any difficulty.

What the problem *does* have is a lot of unreliable outside world: two or three
HTTP APIs, a download client, a filesystem, and a clock, any of which can be
down, slow, or lying. That is what the hexagonal boundaries are for. The workflow
is written against ports, so it can be driven end to end in a test with nothing
but a temp directory.

## Why screaming architecture

The top-level directory listing is the feature list:

```
tracker  torrent  library  staging  seeding  repair  web
```

Not `models/ services/ controllers/ repositories/`. Somebody opening this
repository should be able to tell what it does before reading any code, and
somebody changing "how matching works" should have one obvious place to go.

Each capability then has the same four-part shape internally — `domain`, `ports`,
`application`, `adapters` — so the layering is still explicit, just nested inside
the thing it serves rather than smeared across the top level.

One naming decision worth stating: the download-client capability is `seeding`,
not `qbittorrent`. qBittorrent is an adapter behind `seeding::TorrentClient`. The
directory names what SeedMedic is trying to achieve, not which vendor currently
achieves it.

## Why a durable state machine

Every step of a repair either talks to a network service or writes to a disk, and
the process can die between any two of them. Holding the workflow in memory —
an `async fn` that awaits its way from discovery to seeding — means a restart
loses everything it knew, including whether it already added the torrent.

So the job is a row, its state is a column, and every step is: read the state,
do one thing, record it. The recording is a compare-and-swap in the same
transaction as the audit row, so there is never a moment where the side effect
happened and the record did not, that a replay cannot resolve.

The states are the completed work, not the intended work:

```
discovered → torrent_fetched → matched → staged → injected
           → rechecking → verified → seeding → completed
```

with `awaiting_review` and `failed` off to the side. `awaiting_review` remembers
which state it came from, so resuming cannot skip a step.

### Crash recovery in three parts

- **Leases.** A worker claims a job with an expiring lease. If it dies, the lease
  expires and the job becomes claimable again. There is no queue to rebuild —
  the claimable set is a query over the same table.
- **Replay.** Every side effect is idempotent and every transition is a CAS, so
  re-running a step is harmless.
- **Reconciliation.** Reality may have changed while we were down. On startup
  each unfinished job is walked *backwards* to the last state that is still
  true; a step can do the same mid-flight with `StepOutcome::Rewind`.
  Reconciliation never moves a job forwards, because external state cannot prove
  we are the ones who put it there.

## Why configuration is reloadable

A repair job is durable and idempotent by design (see above); a running
configuration was not, until `docs/todos/0016-a-swappable-runtime.md`. That
mattered once settings could be changed from a browser: a first-run flow that
ends at "now go find a terminal and `docker restart`" is not a first-run flow.

The reload is not a new mechanism. It is the same startup sequence —
`bootstrap::open` once, `bootstrap::build` to wire one generation, reconcile,
spawn a worker — run again, against the same durable state that already makes
killing the process at any moment safe. Two properties fall out of that reuse
rather than needing new machinery:

- **Build before stop.** `RuntimeHandle::reload` builds the new generation
  first; only once that succeeds does it stop the old worker. Both the config
  load and the build can fail, and both leave the previous generation
  untouched and still serving — a failed reload is a pure no-op, because
  nothing observable happens until the replacement is ready.
- **One generation per worker task.** A worker keeps the `Arc<Runtime>` it was
  spawned with for its whole life; it is never handed a new one mid-tick.
  Combined with build-before-stop, this means "which configuration was a step
  running under" is always answerable, and a request in flight when a reload
  lands finishes against the generation it started with.

Two pieces of process state are `bootstrap::Persistent` rather than part of a
generation, because a reload rebuilding either would itself be an outage: the
database connection (`database.path` cannot change without a restart), and the
two things an operator was looking at right before they saved a setting —
`WorkerHealth` (so `/health` does not dip) and `Diagnostics` (so a tracker's
error history is not the price of fixing it).

Not every setting can be applied this way. Changing `staging.root` or removing
a tracker while a job has data staged under the old value does not have a safe
in-place meaning — the same aliasing hazard the review actions guard against —
so those reloads are refused outright, before anything is built, rather than
applied halfway.

## Where safety lives

Mostly in types, so it is hard to get wrong by accident:

- `SafeRelativePath` — a torrent path that has been proven to be a plain relative
  path. There is no other way to build a destination.
- `StagingRoot` — a directory proven at startup not to overlap the library.
- `MaterializationStrategy::aliases_library_file()` — the one bit that decides
  whether resuming could write into somebody's media.
- `MatchConfidence` — ordered, so policy expresses a floor, and `Exact`
  unreachable without piece verification.
- `AutoResume` — has no `Always` variant, so "resume unverified data" is not a
  state the configuration can express.

And partly in one pure function, `repair::policy::assess_data`, which is the
rule that no configuration can weaken: incomplete data that shares inodes with
the library never goes near a running torrent.

## What is deliberately absent

- No blob store: `.torrent` bytes are a column, so acquiring one is atomic with
  the transition that records it.
- No generic repository: `RepairStore` has exactly the methods the workflow
  calls.
- No abstraction over "notifier", "scheduler", or "event bus". There is a `tokio`
  interval and a `tracing` subscriber.
- No second crate, no workspace. One deployable, one package.

## Current state

Feature-complete. The workflow, the store, the state machine, staging, matching,
the operator UI and configuration are implemented, and so are the external
integrations — Unit3D, qBittorrent, Sonarr/Radarr, bencode decoding, reflinks.
The fake tracker and client adapters remain, behind the `fakes` feature, because
they are what makes the whole workflow testable without a network and what makes
`/settings`'s "Load demo configuration" able to run a real repair end to end.

The one thing in flight is the operator UI, which
`docs/todos/0021-a-react-operator-ui.md` is rebuilding as a React client over a
JSON API. That reverses this document's own "no asset pipeline" position; see
0021's cost accounting for what it buys and what it costs.
