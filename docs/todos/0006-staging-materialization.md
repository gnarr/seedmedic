# 0006 — Reflinks, cross-device handling, and free space

**Status:** Not started
**Depends on:** 0003
**Blocks:** 0008

## Problem

`LocalStaging` implements hardlink and copy. Reflink — the strategy SeedMedic
actually wants, and the default in `config.example.toml` — reports itself
unavailable, so every repair falls through to a full copy.

That is safe but expensive: repairing a 60 GB season pack duplicates 60 GB. On
btrfs, XFS, and bcachefs a reflink costs nothing and is *safer* than a hardlink,
because a write to the staged file allocates new extents instead of modifying
the library file. Getting this working removes the main reason an operator would
be tempted to enable `allow_hardlink`.

Three related gaps come with it:

- Nothing checks free space before copying. A repair can fill the staging
  filesystem and take the rest of the system with it.
- A cross-device hardlink fails with `EXDEV` and is reported as
  `StrategyUnavailable`, which is correct but only discovered per file, after
  the plan is underway.
- `existing_strategy` guesses from link count when re-inspecting a staged file.
  It is deliberately pessimistic, but a job row that says `reflink` and a file
  that is really a hardlink would be a safety problem, so the guess should be
  narrowed or removed.

## Architectural context

`src/staging/AGENTS.md` has the rules. The relevant ones here: never silently
downgrade to a different strategy than the caller believes it got, and
`aliases_library_file()` must be right, because the resume guard trusts it.

`MaterializationPolicy::preference()` produces the ordered list of strategies to
try. `StrategyUnavailable` means "try the next one"; anything else is fatal for
that file.

## Expected behaviour

- On a filesystem that supports it, reflinking succeeds and the job records
  `materialization = reflink`.
- On one that does not, reflink reports `StrategyUnavailable` and the next
  permitted strategy is used — same as today, but for a real reason.
- Support is probed once per (source device, staging device) pair, not attempted
  per file, so a 500-file torrent does not make 500 doomed syscalls.
- Before a plan that will copy, free space on the staging filesystem is checked
  against the plan's total size plus a configurable margin. Insufficient space
  parks the job for review rather than filling the disk.
- Cross-device situations are detected while planning, so the strategy is chosen
  once with a clear reason recorded, not discovered file by file.

## Implementation steps

1. **Add `reflink-copy`.** It handles `FICLONE` on Linux, and the equivalents
   elsewhere, and is the boring proven choice. Do not hand-roll the `ioctl`.

2. **Implement the `Reflink` arm** of `attempt`. Map "filesystem does not
   support this" and "different devices" to `StrategyUnavailable`; map a genuine
   I/O failure to `Io`. These must not be confused: the first falls through, the
   second must not.

3. **Probe once.** Add a `probe` step that, for the plan's source device and the
   staging device, determines which strategies can work — same device for
   hardlink, reflink support for reflink. Cache it per device pair for the
   process lifetime; a mounted filesystem does not change its mind. Record the
   probe result in the `staged` transition's audit detail.

4. **Free-space check.** Before materialising, sum the plan's bytes for files
   that will be copied (reflinked and hardlinked files cost nothing) and compare
   against available space, minus `staging.min_free_bytes` from config. Insufficient
   space is a new `StagingError` variant mapping to a new `ReviewReason`, so the
   operator sees "not enough disk" rather than a generic I/O error. Use `statvfs`
   via `libc`, or `nix`, or an existing dependency — do not add a crate for one
   syscall if something already in the tree provides it.

5. **Narrow `existing_strategy`.** Options: drop it and have `materialize`
   re-derive nothing, returning the strategy from the job row instead; or keep
   the pessimistic `nlink > 1 → Hardlink` guess and document that a re-inspected
   file is never trusted to be safer than it looks. The second is what the code
   does now; make it a deliberate, documented choice or remove the function.

6. **Verify the staged result.** After materialising, confirm each destination
   has the expected size. Cheap, and it catches a short write before the
   torrent is injected and rechecked.

## Invariants and safety constraints

- Never silently downgrade. `StrategyUnavailable` falls through *within* the
  policy's permitted list; nothing outside it is ever used.
- A reflink is not a hardlink. `aliases_library_file()` stays `false` for
  `Reflink` and `true` for `Hardlink`, and the probe must not conflate them.
- The library is still read-only. A reflink source is opened for reading.
- Free space is checked before writing, not discovered during.
- Materialisation stays idempotent: an existing destination at the right size is
  left alone.

## Likely files

- `src/staging/adapters/local.rs`
- `src/staging/domain.rs` (probe result type)
- `src/staging/ports.rs` (new error variant)
- `src/repair/domain.rs` (new `ReviewReason`)
- `src/repair/application/stage.rs`
- `src/config.rs`, `config.example.toml`
- `Cargo.toml`

## Required tests

- On a filesystem supporting reflinks, a plan reflinks and the layout reports
  `Reflink` and `aliases_library_files() == false`.
- On one that does not, the plan falls through to copy — and the test skips
  rather than fails when the CI filesystem cannot reflink. Detect and skip
  explicitly; a silently-skipped test is worse than none.
- A source on a different device than staging never produces a hardlink.
- A plan larger than available space parks for review, and nothing is written.
- The probe runs once for a many-file plan — assert on a call counter.
- Re-materialising an already-staged file does not rewrite it (existing test
  still passes).
- A staged file whose size does not match after materialisation is an error.

## Acceptance criteria

- On btrfs or XFS, a 60 GB repair uses no additional disk space.
- On ext4, the same repair copies, and says so in the audit trail.
- Filling the staging disk is not possible through normal operation.
- `allow_hardlink` remains off by default and there is no longer a good reason
  to turn it on.

## Out of scope

- Deduplicating between repair jobs.
- Cleaning up staging for completed jobs — worth doing, and it belongs with 0010
  where the operator decides retention.
- Sparse files, compression, or anything clever about the copy itself.

## Open questions

- How to detect reflink support without attempting one? Attempting a zero-length
  clone into a temp file in the staging root is the pragmatic answer; is there a
  cleaner one?
- Where to get `statvfs` — is a new dependency justified, or is the `libc` call
  small enough to write inline?
- Should `min_free_bytes` be absolute, a percentage, or both? Absolute is
  predictable; a percentage scales with the disk.
- If a torrent is 60 GB and only 10 GB is missing from the client's perspective,
  is there value in staging only the missing files? That would need per-file
  knowledge of what the client already has, which is 0008 territory — note it
  there if so.
