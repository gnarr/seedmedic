# AGENTS.md — `src/staging`

Supplements the root `AGENTS.md`. This is the only module in SeedMedic that
writes to a filesystem. Everything here is written on the assumption that a bug
costs somebody their media library.

## The three rules

1. **The media library is opened read-only.** `std::fs::copy`, `hard_link`, and
   metadata calls, nothing else. No `OpenOptions::write`, no `remove_file`, no
   `rename` with a library path on either side.

2. **Writes happen under a validated `StagingRoot`, or not at all.**
   `StagingRoot::new` proves at startup that the root is absolute, exists, and
   does not overlap any configured library root in either direction. Anything
   that takes a destination takes a `SafeRelativePath` and resolves it with
   `safety::resolve_under`.

3. **No path is touched through a symlink.** `SafeRelativePath` is syntactic
   only — it cannot know that `job-1` is a symlink to somebody's home directory.
   `resolve_under` checks every existing component, and `create_directories`
   creates one level at a time and re-checks after each, rather than letting
   `create_dir_all` descend through a link that appeared between check and use.

## Materialisation strategies

| Strategy | Space | Shares fate with the library |
|---|---|---|
| `Reflink` | Free | No — a write to the staged file allocates new extents |
| `Hardlink` | Free | **Yes — it is the same inode** |
| `Copy` | Full size | No |

`MaterializationStrategy::aliases_library_file()` is the predicate the resume
guard keys off. If you add a strategy, get that method right first: an aliasing
strategy that reports `false` is the single worst bug this codebase could have.

Preference order comes from `MaterializationPolicy::preference()` and always puts
`Hardlink` last, because everything else is safer.

A strategy that is unavailable — wrong filesystem, not implemented — returns
`StagingError::StrategyUnavailable` so the caller falls through to the next
permitted one. Anything else is a hard error: **never silently downgrade** to a
different strategy than the one the caller believes it got, because the job row
records the strategy and the resume decision trusts it.

## Idempotency

`materialize` must be safe to run again. A destination that already exists at
the expected size is left alone, so a retry after a crash resumes rather than
restarts. A destination at the *wrong* size is our own half-written leftover and
is removed — that is the only file deletion in the module, and it is confined to
a path already proven to be under the staging root.

`inspect` answers presence only. It deliberately does not try to re-derive which
strategy each file used: reflinks are not detectable portably, and guessing
would undermine the resume guard. The job row is the record.

## What `discard` may delete

A job's own staging directory, resolved through `resolve_under`, after
confirming the result still starts with the staging root. Nothing else, ever.
`discard` is not a general "clean up" function; do not grow it into one.

## Testing

Every test here uses real temporary directories. If you add a rule, add a test
that creates the dangerous situation on disk — a symlink, a size mismatch, a
missing source — and proves the code refuses. Assertions that the library file
is still intact afterwards are cheap and worth writing.
