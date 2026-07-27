# 0004 — Sonarr and Radarr candidate discovery

**Status:** Not started
**Depends on:** 0003
**Blocks:** 0005

## Problem

The only working candidate source is `FilesystemCandidateSource`, which walks a
library root and returns every file of a matching size. That works, but it is
blind: it cannot tell that `/media/tv/Show/Season 01/Show - S01E01.mkv` is the
episode a torrent called `Show.S01E01.1080p.WEB-DL.x264-GRP` wants, and on a
large library it produces size collisions that push jobs into manual review.

Sonarr and Radarr already know exactly which file corresponds to which release,
because they imported it. `library::adapters::arr::ArrCandidateSource` is a stub
that fails every call.

## Architectural context

`library::CandidateSource` has one method: given the torrent name and its file
list, return plausible library files. Sources are additive — the workflow queries
all of them and matches over the union — so an *arr being down degrades match
quality rather than breaking the repair.

Matching itself (`library::matching`) is deliberately separate and deterministic.
This document is about *finding* candidates; deciding between them is 0005.

## Expected behaviour

- Given a release name, the adapter finds the corresponding series or movie and
  returns the paths of its files, with their sizes.
- For a season-pack torrent, it returns every episode file of that season, not
  just one.
- Candidates carry `CandidateOrigin::Sonarr { instance }` / `Radarr`, so the
  audit trail can say *why* a file was chosen — "Sonarr says this is S01E03" is
  an explanation; "a file of the right size existed" is not.
- A instance that is down or misconfigured returns a transient error and is
  skipped, without failing the step.
- Nothing here ever writes to an *arr. These are read-only lookups.

## Implementation steps

1. **Share the HTTP client** built in `bootstrap` (see 0002). Add `reqwest` if
   0002 has not already.

2. **Work out the lookup path.** Both APIs support parsing a release name:
   `GET /api/v3/parse?title=<release>` returns the matched series/movie and, for
   Sonarr, the episodes. From there, `GET /api/v3/episodefile?seriesId=` or
   `GET /api/v3/moviefile?movieId=` yields paths and sizes. Confirm the endpoint
   shapes against the version you target and record the minimum supported *arr
   version.

3. **Fall back sensibly.** If `parse` finds nothing, try a title lookup, or give
   up and return an empty `Vec` — an empty result is honest here, because the
   *arr genuinely does not know this release. Only return an error when the
   request itself failed.

4. **One adapter, two kinds.** Sonarr and Radarr differ in endpoint names and
   response shapes but not in structure. Keep `ArrCandidateSource` with an
   `ArrKind` discriminant rather than two near-identical adapters, unless the
   differences turn out to be larger than expected — in which case split, and
   say why here.

5. **Path translation.** The *arr may report paths as its container sees them
   (`/tv/Show/...`) while SeedMedic sees `/srv/media/tv/Show/...`. This is the
   single most common source of "it worked for me" bug reports in this genre.
   Add an optional per-instance `path_mappings` list to the config and apply it
   to every returned path. Verify the mapped path exists and matches the reported
   size before returning it as a candidate — a candidate we cannot open is worse
   than no candidate.

6. **Index the filesystem source.** While here: `FilesystemCandidateSource` walks
   the whole root on every query. Decide whether that is still acceptable (it
   probably is — repairs are rare and the walk is `stat`-only) or whether it
   wants a cached size index. Do not build a cache without measuring first.

7. **Delete the stub** and the `const TODO`.

## Invariants and safety constraints

- Read-only. No `POST`, `PUT`, or `DELETE` to any *arr endpoint.
- API keys are `config::Secret` and never logged, including in URLs.
- A path returned as a candidate must exist and have the size reported. Verify
  before returning.
- A failed lookup is an error; a successful lookup that found nothing is an empty
  list. Never conflate them — the matching step treats them differently.
- Paths from an *arr are still external input. They become `PathBuf` for reading
  only, and never become a staging destination.

## Likely files

- `src/library/adapters/arr.rs`
- `src/library/adapters/filesystem.rs` (only if indexing is justified)
- `src/config.rs` (`path_mappings`)
- `src/bootstrap.rs`
- `config.example.toml`
- `Cargo.toml`

## Required tests

Wiremock:

- A Sonarr `parse` response for a season pack yields one candidate per episode
  file, with sizes.
- A Radarr `parse` response for a movie yields the movie file.
- An unmatched release yields an empty `Vec`, not an error.
- A `500` yields a transient error.
- A `401` yields a non-transient error.
- Path mapping rewrites a container path to a host path.
- A mapped path that does not exist on disk is dropped, with a warning, rather
  than returned.
- No API key appears in any error message.

## Acceptance criteria

- With a Sonarr instance configured, a season-pack hit-and-run matches every
  episode without manual review.
- With the instance stopped, the same repair still runs and falls back to the
  filesystem source, logging the degradation.
- The audit trail records which source produced each chosen candidate.

## Out of scope

- Lidarr, Readarr, Whisparr. The port is the same shape; add them when somebody
  needs them.
- Triggering an *arr import, rename, or search.
- Using *arr metadata to *improve confidence* — that is 0005. This document only
  supplies candidates.

## Open questions

- Which *arr API version to target, and what is the oldest that works?
- Does `parse` reliably handle the release-name formats private trackers use, or
  is a title-plus-season fallback needed in practice?
- Should path mappings be per-instance or global? Per-instance is more correct
  and more configuration; global covers the common single-container case.
- If Sonarr reports a file whose size differs from the torrent's by a few bytes,
  is that worth surfacing as a near-miss in the review UI rather than dropping
  silently? It usually means a different release of the same episode.
