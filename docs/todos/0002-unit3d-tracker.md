# 0002 — Unit3D tracker adapter

**Status:** Not started
**Depends on:** nothing
**Blocks:** 0009

## Problem

`src/tracker/adapters/unit3d.rs` fails every call with `NotImplemented`, so
SeedMedic cannot discover a real hit-and-run. This is the first real tracker
integration and the one that proves the `TrackerClient` port is the right shape.

Unit3D is the tracker software behind Blutopia, Aither, and a large family of
related sites. They share an API surface but differ in details, so the adapter
must be configurable enough to serve more than one instance without becoming a
per-site special case.

## Architectural context

`tracker::TrackerClient` has exactly three methods, because a repair needs
exactly three things: find the warnings, get the torrent, ask whether a warning
is gone. `src/tracker/AGENTS.md` explains why that must not grow.

The workflow treats `TrackerError::is_transient()` as the decision between
"retry with backoff" and "park for review", and treats
`HitAndRunStatus::Unknown` as "keep seeding and keep asking". Getting those
classifications right matters more than covering every endpoint.

## Expected behaviour

- `list_hit_and_runs` returns every outstanding warning for the configured
  account, with the tracker's torrent id, the release name, the size, the
  info-hash where available, and the deadline where available.
- `fetch_torrent_file` returns the raw `.torrent` bytes, following whatever
  download-link scheme the instance uses, authenticated with the configured key.
- `hit_and_run_status` distinguishes cleared, still-outstanding, and
  not-interpretable, and never guesses `Cleared`.
- Rate limiting is respected: `429` and `Retry-After` become
  `TrackerError::RateLimited`, and the adapter does not retry internally.
- A `401`/`403` becomes `TrackerError::Unauthorized`, which is not transient, so
  a bad key parks jobs for a human instead of retrying forever.

## Implementation steps

1. **Add `reqwest`** (`default-features = false`, `features = ["json",
   "rustls-tls"]`) and `wiremock` as a dev dependency. Build one shared
   `reqwest::Client` in `bootstrap` with a `seedmedic/<version>` user agent and
   pass it in, rather than one client per adapter.

2. **Model the API responses** as private `#[derive(Deserialize)]` structs inside
   the adapter. Do not let a tracker's JSON shape escape into the domain — map
   into `HitAndRun` at the boundary and drop everything a repair does not need.

3. **Implement `list_hit_and_runs`.** Unit3D exposes the user's torrent history;
   identify the endpoint and the field that marks a hit-and-run, and page
   through it. If the tracker returns a page shape we do not recognise, return
   `Protocol`, never an empty list.

4. **Implement `fetch_torrent_file`.** Verify the response is actually a
   `.torrent` (content type, or a leading `d` for a bencode dict) before
   returning it — an HTML error page with a 200 status is a real failure mode on
   these sites, and passing it to the inspector would produce a confusing
   `Malformed`.

5. **Implement `hit_and_run_status`.** Prefer a per-torrent endpoint over
   re-listing everything. Map "no longer flagged" to `Cleared`, "still flagged"
   to `Active`, and anything else to `Unknown`.

6. **Authentication.** Unit3D uses an API token. Decide whether it goes in a
   header or a query parameter per instance and make it configurable if the
   family disagrees. The token comes from `config::Secret` and must never be
   logged or included in an error message — including the URL in a
   `Transport(...)` string, if the token is a query parameter.

7. **Rate limiting.** A simple per-adapter minimum interval between requests is
   probably enough given the workflow's pace. Do not add a token-bucket crate
   without evidence it is needed.

8. **Delete the stub** and the `const TODO`.

## Invariants and safety constraints

- Never fabricate a warning or a clearance. An unparseable response is an error.
- Never log the API token, in any form, including inside a URL.
- Torrent names and any path-like strings stay `String` inside this module. They
  reach the filesystem only via `torrent::SafeRelativePath`, elsewhere.
- Classify errors honestly — `is_transient()` decides whether a human gets
  involved.
- Respect the tracker's limits. A ban for hammering is a worse outcome than an
  unrepaired hit-and-run.

## Likely files

- `src/tracker/adapters/unit3d.rs`
- `src/tracker/ports.rs` (only if a genuinely missing error variant appears)
- `src/config.rs` (per-instance authentication options)
- `src/bootstrap.rs` (shared HTTP client)
- `Cargo.toml`
- `config.example.toml`

## Required tests

Wiremock, against recorded response shapes:

- A listing with two warnings maps to two `HitAndRun` values with the right
  fields.
- An empty listing returns an empty `Vec` (the tracker genuinely says none).
- A malformed listing returns `Protocol`, not an empty `Vec`.
- `401` returns `Unauthorized` and `is_transient()` is false.
- `429` with `Retry-After: 30` returns `RateLimited { retry_after_seconds: 30 }`
  and `is_transient()` is true.
- `fetch_torrent_file` on an HTML error page with a 200 status returns an error.
- A status response that means nothing to us returns `Unknown`, not `Cleared`.
- No test fixture or error message contains the token.

## Acceptance criteria

- Configuring `kind = "unit3d"` against a wiremock instance discovers warnings,
  downloads a torrent, and reports clearance.
- The adapter never panics on malformed input; every path returns a typed error.
- Paging works for an account with more warnings than one page holds.
- The stub and its `NotImplemented` are gone.

## Out of scope

- Other tracker families. Gazelle, MAM, and HTML-scraped trackers each want
  their own document, written when somebody actually needs one.
- Uploading, searching, or anything else Unit3D can do.
- Automatic tracker discovery or credential provisioning.

## Open questions

- Which endpoint reliably lists hit-and-runs across the family, and is it the
  same on every instance? If not, what is the minimum per-instance
  configuration — a path template?
- Do all instances agree on token placement (header vs query)?
- Is the info-hash present on the listing, or does it only arrive with the
  `.torrent`? The job model already tolerates its absence, but knowing avoids a
  round trip.
- Does the API expose the required seed time and elapsed time? If so, the UI
  could show how close a repair is, which would be worth a field on `HitAndRun`.
- Is there a per-account request budget worth respecting explicitly?
