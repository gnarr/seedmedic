# 0005 — Piece verification and matching confidence

**Status:** Not started
**Depends on:** 0003, 0004
**Blocks:** 0010

## Problem

`MatchConfidence::Exact` is currently unreachable. `library::matching` can only
compare sizes and filenames, so the best it will ever say is `Probable`, and the
safety rule "exact file size is evidence, not proof" means it must stay that way
until something actually verifies content.

The consequence is real: two different encodes of the same episode can be
byte-identical in length, and a same-size different-content match would be
staged, fail the recheck, and park — wasting a full copy of the file. Worse, with
hardlinks permitted, the recheck failure is precisely the situation the resume
guard exists for, and we would rather not get there at all.

Piece verification closes this. A torrent's `pieces` array is a SHA-1 per piece;
hashing a candidate's bytes at a few piece boundaries proves the content matches
without reading the whole file.

## Architectural context

`library::matching::plan_matches` is a pure function: torrent files plus
candidates in, a `MatchPlan` out. `repair::policy::decide_match` then decides
whether the plan clears the configured confidence floor. That split stays.

Piece verification breaks purity — it reads files. So it belongs as a *separate
stage* the matching step runs on the plan, not inside `plan_matches`. Keep the
deterministic selection pure and testable; add verification around it.

## Expected behaviour

- After selection, each proposed match is verified by hashing one or more pieces
  that fall entirely within that file, and comparing to the torrent's `pieces`.
- A file that verifies is `MatchConfidence::Exact` with
  `MatchEvidence.piece_verified = true`.
- A file that fails verification is *not* downgraded — it is removed as a
  candidate, and selection is retried without it. A file that hashes wrong is
  the wrong file.
- Verification is bounded: a configurable number of pieces per file, not the
  whole torrent. The full check is qBittorrent's job at recheck time.
- Pieces spanning a file boundary in a multi-file torrent are skipped rather
  than mishandled — a piece can cover the tail of one file and the head of the
  next, and can only be verified once both are chosen.
- With verification unavailable (no `pieces` data, unreadable candidate), the
  result is today's behaviour: at most `Probable`.

## Implementation steps

1. **Get the piece hashes.** Either from `TorrentMetadata` if 0003 stored them,
   or by re-inspecting the persisted `.torrent` bytes. Decide and record.

2. **Work out piece-to-file mapping.** Files are concatenated in order; piece
   *n* covers bytes `[n * piece_length, (n+1) * piece_length)` of that stream.
   Write this as a pure function with its own tests — off-by-one errors here
   produce confident wrong answers, which is the worst failure mode available.

3. **Choose which pieces to check.** First, last, and middle piece fully inside
   the file is a reasonable default and catches truncation, wrong-encode, and
   wrong-episode. Make the count configurable
   (`policy.verification_pieces`, default 3, 0 to disable).

4. **Hash off the async runtime.** Reading and hashing is blocking I/O and CPU;
   use `spawn_blocking`. Read only the byte ranges needed — `seek` plus a bounded
   read, not the whole file.

5. **Retry selection on failure.** If a candidate fails verification, drop it and
   re-run `plan_matches` on the remaining candidates. Bound the loop. Record every
   rejection in the evidence — "we tried this file and it hashed wrong" is
   valuable in review.

6. **Improve name comparison while here.** `names_agree` is exact,
   case-insensitive equality, which fails for the common case of a library file
   renamed by an *arr. Now that verification can confirm a guess, looser name
   matching becomes safe: normalise separators and case, strip the extension, and
   compare. Do not add a fuzzy-distance crate unless a test case demands it.

7. **Extend the audit detail.** The `matched` transition's `detail` should record
   which pieces were checked and what was rejected.

## Invariants and safety constraints

- `Exact` requires verified bytes. Nothing else may produce it.
- A verification failure removes a candidate. It never downgrades confidence and
  proceeds anyway.
- Reading a candidate is read-only, uses bounded ranges, and never follows a
  symlink outside a library root.
- Verification is best-effort: unavailable data means lower confidence, never a
  failed repair.
- Selection must stay deterministic. Same inputs, same plan — including the
  order candidates are rejected in.

## Likely files

- `src/library/matching.rs`
- `src/library/verification.rs` (new — the impure part)
- `src/library/domain.rs` (`MatchEvidence` fields)
- `src/repair/application/match_media.rs`
- `src/repair/policy.rs` (`verification_pieces`)
- `src/config.rs`, `config.example.toml`

## Required tests

- Piece-to-file mapping: single file, multi-file, a piece spanning a boundary,
  a file smaller than one piece, a final short piece.
- A candidate with the right bytes verifies to `Exact`.
- A candidate with the right size and wrong bytes is rejected, and a correct
  second candidate is chosen instead.
- With every candidate rejected, the plan is incomplete and the job parks.
- With `verification_pieces = 0`, behaviour matches today's.
- An unreadable candidate degrades to `Probable`, not an error.
- Verification of a 1 GiB file reads far less than 1 GiB — assert on bytes read.
- Setting `min_match_confidence = "exact"` now yields completed repairs rather
  than universal review.

## Acceptance criteria

- A library containing two same-size files, one correct, produces the correct
  match automatically.
- `min_match_confidence = "exact"` is a usable setting.
- Verification time is proportional to piece count, not file size.

## Out of scope

- BitTorrent v2 merkle verification. If 0003 rejected v2 torrents, this stays
  v1-only.
- Verifying the whole torrent — that is what the qBittorrent recheck does, and
  duplicating it wastes hours of disk.
- Repairing partial matches by fetching the missing pieces from the swarm.

## Open questions

- Store `pieces` on `TorrentMetadata` or re-parse the persisted bytes on demand?
  Re-parsing keeps the type small; storing avoids re-decoding on every retry.
- Three pieces per file: enough? A truncated-then-padded file would pass a
  first-and-last check but fail a middle one; is there a case that defeats all
  three?
- Should a piece spanning two candidate files be verified once both are
  provisionally chosen? It is the strongest possible evidence, but it couples the
  two files' fates.
- Is name matching worth loosening at all once verification exists, or should
  selection lean entirely on size plus verification?
