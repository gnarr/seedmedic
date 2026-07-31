# 0021 — A React operator UI

**Status:** In progress — the operator surface is done; `/settings` is not
**Depends on:** 0016, 0017, 0018, 0019
**Blocks:** nothing

## Problem

The operator UI is unusable on a phone, has no navigation, and cannot show a
repair progressing without a manual reload.

Concretely, and all of it verifiable in the tree this document was written
against:

- **There is no navigation at all.** `layout::page`'s header is a wordmark, a
  tagline, and a sign-out button. `/status` and `/settings` are reachable only by
  typing the URL — there is no link to either from `/`.
- **The entire settings surface is unstyled.** `web/settings/render.rs` emits
  `.field`, `.help`, `.error`, `.row`, `.sub`, `.secret-status`, `.unit`,
  `.muted`, `.confirm` and `.clear`; none of those selectors exist in
  `layout.rs`'s `STYLE`, and neither does any rule for `input`, `select`,
  `textarea`, `label` or `fieldset`. A validation error renders in body colour.
- **The stylesheet has no responsive rule of any kind** — 33 declaration lines,
  five custom properties, and `@media (prefers-color-scheme: dark)` as its only
  media query. Against that: a six-column file table, a history table with
  pretty-printed JSON inside a `<pre>` inside a cell, and
  `dl { grid-template-columns: max-content 1fr }` unconditionally. Buttons are
  `padding: .4rem .9rem` at a 15px base — about 34px tall, against the 44px
  touch-target minimum.
- **Nothing updates.** `jobs::rechecking_notice` and
  `jobs::seeding_progress_notice` exist precisely so a long step is not silently
  pending forever, and both change only when the operator presses reload.
- **Nothing confirms.** All six single-job review actions return a bare 303. There
  is no success message anywhere, and no positive `.notice` variant exists — so a
  connection test that *passed* renders in the same amber warning box as one that
  failed.
- The four state colours are hard-coded eight times and are identical in light
  and dark, so `#2f855a` on `#161616` fails contrast.
- A `failed` job is a dead end in the browser: `validate_transition` permits
  `Failed → Discovered` and `POST /jobs/{id}/restart` would accept it, but
  `review_panel` renders only for `awaiting_review`.
- `MatchEvidence` is persisted for every planned file and displayed nowhere,
  even though it is exactly the "why do we believe this" an operator needs before
  approving an ambiguous match. `next_attempt_at`, `resume_approved` and
  `consecutive_unknown_tracker_status` are likewise read and never rendered.

None of that is fixable inside the current constraint. Live updates need a client;
a phone-usable file plan needs a card layout at one breakpoint and a table at
another; and a settings surface of 49 fields across nine pages needs components
rather than nine hand-written `html!` blocks.

## Architectural context

**This document overturns a constraint that three places assert.** They are, in
full, so that nobody has to go looking:

- `src/web/AGENTS.md`: "No JavaScript, no asset pipeline. One inline stylesheet,
  in `layout.rs`."
- `src/web/mod.rs`'s module doc: "Server-rendered, no JavaScript, no API surface
  beyond what the pages need."
- `docs/todos/0017-the-settings-pages.md`: "The operator UI is server-rendered
  `maud`, no JavaScript, one inline stylesheet, no asset pipeline, and no API
  surface beyond what the pages need. Those constraints hold. They are what make
  this a few hundred lines rather than a frontend."

Each of those is amended in place with a pointer here, in the same way 0017 is
amended by 0020 and 0011 is amended by 0016/0017/0019. The claims were true and
the reasoning was sound; what changed is that the product needs two things — a
UI that works on a phone, and a UI that updates itself — which that constraint
cannot deliver at any amount of effort.

**What does not change.** `web/settings/fields.rs` is untouched but for one new
entry. `web/settings/save.rs`'s six-step pipeline, `EMPTY_MEANS_ABSENT`, the
`Kind::SecretFile` refusal, the env/file-sourced-secret skip, the danger/confirm
guard, `validate`, `ConfigDocument`, and `Config::problems()` all keep their
current behaviour, and most of them keep their current source. The web module
still contains no rules of its own — and gains a sharper version of that rule,
because the client is now a separate program that could disagree: **the client
does no validation and re-derives no rule the server can send it.** That is why
`FIELDS` is served as JSON rather than restated in TypeScript, and why job detail
returns a server-computed map of which actions are legal rather than letting the
client work it out from `state`.

**The real cost of this change is not the Node toolchain.** It is that four enums
whose exhaustiveness is load-bearing safety machinery stop being exhaustively
matched. Today, adding a `Kind` variant will not compile until both
`render.rs::input` and `save.rs::parse_field` handle it; adding a `ReviewReason`
will not compile until `description()` handles it. Across a language boundary a
missing case renders blank and ships. `Kind`, `RepairState`, `ReviewReason` and
`SecretSource` are the four. Step 2 buys that back: a Rust test generates the
TypeScript unions from the Rust enums and fails if the committed file differs, and
every TypeScript `switch` over them ends in a `never` assertion — so adding a
variant fails `cargo test` until regenerated, then fails `tsc` until handled.

**Two guarantees are currently enforced by the absence of code, not by a test.**
Both are one line from being destroyed, and neither is caught by anything that
exists today:

1. `Serialize` appears **zero times** in `src/config/mod.rs`. `Config` derives
   only `Deserialize`, and `Secret` has no `Serialize` impl — so "a secret leaks
   through the JSON boundary" is a *compile error*. `impl Serialize for Secret`
   is outside `src/web/`, so the `.expose(` grep never sees it, `clippy` is happy,
   and every inline secret then ships in `GET /api/v1/settings`.
2. `SafeRelativePath` has a **hand-written** `Deserialize` that re-runs
   `parse()`, and that is the shape to keep — deserialisation is not the hazard,
   a *derived* `Deserialize` is. On a newtype over `String` the derive would
   accept `../../etc/passwd` through the very type whose purpose is to promise it
   cannot, which is the rule "nothing joins a torrent-supplied path onto a
   directory except through `torrent::SafeRelativePath`". So the guard has two
   halves: no derive, and the hand-written impl still routes through `parse`.
   (Without the second half, someone could replace its body with
   `Ok(Self(raw))` and the first half would still pass.)

Both get a blunt grep test, in the same deliberately unclever style as the
`.expose(` check, and for the same stated reason: clever is what lets one through
by accident.

**Everything new lives under `src/web/`.** The `.expose(` test walks
`CARGO_MANIFEST_DIR/src/web`, so an API module at `src/api/` would silently escape
the one guard the repository calls "the one thing here that must never regress
silently". That test also currently passes on an empty directory; it gains an
assertion that the walk visited files.

## Expected behaviour

- Every screen the maud UI has, reachable **by clicking**, at 320px.
- A repair advancing is visible without a reload, and the UI is honest about when
  it last heard from the server.
- Every operator action confirms what it did, in the server's own words when it
  refuses.
- The parked-for-review screen is the best screen in the product: the reason in
  prose, the evidence behind each candidate, and one decision at a time.
- Every setting stays viewable and editable, with a bad value producing a message
  next to that field, the operator's other typing preserved, and nothing written.
- A secret's value is still never rendered, logged or echoed. A secret from the
  environment or a `*_file` is still read-only with its source named.
- A first run still explains itself, still lists each unmet setting, and still
  offers the demo configuration.
- `cargo build` with no Node installed still produces a working binary, which
  says how to build the UI rather than 404ing.

## Implementation steps

Additive first, delete last. `cargo fmt`, `clippy --all-targets --all-features -D
warnings`, `cargo test --locked --all-features` and `cargo build --locked
--no-default-features` are clean after every step, and the app is usable
throughout — the maud UI keeps serving `/` until step 11.

1. This document, the three amendment blocks, and the pre-existing staleness in
   `docs/architecture.md` (its "Current state" still calls the external adapters
   loud stubs, which `README.md` contradicts).
2. **The guards, before the code they guard.** The two grep tests, the
   whole-router sentinel test, the `.expose(` visited-files assertion, and a route
   inventory.
3. Store aggregates, migration `0007`, `fail_at.rs` forwarding, and `web/metrics.rs`
   off `jobs(i64::MAX)`.
4. `src/events.rs` — `EventBus` on `Persistent`, cloned into `RepairDeps`,
   publishers wired, nothing subscribing yet.
5. Read-only `/api/v1`: session, dashboard, diagnostics, jobs.
6. Settings JSON, as two commits: (a) refactor `save.rs` alone, behind a
   deliberately temporary shim, so **all four settings test files pass
   unchanged** — that is the proof the refactor preserves behaviour; (b) add the
   endpoints, with the settings sentinel test in the same commit.
7. SSE.
8. CSRF token and session hardening.
9. `web/` scaffold, `spa.rs`, the placeholder bundle, `server.base_path`, served
   at `/app/*`.
10. Screens, one commit each, each with its unit and browser tests.
11. **The cutover.** `/` serves the shell; every maud route and renderer is
    deleted; the eight HTML-asserting test files are rewritten.
12. Packaging, CI, docs.

## Invariants and safety constraints

Unchanged, and each with a test that fails without it:

- Nothing under `src/web/` calls `Secret::expose`. **New:** nothing under `src/`
  gives `Secret` a `Serialize` impl, and nothing gives `SafeRelativePath` a
  `Deserialize` impl.
- No secret value appears in any response from any route, from any source
  (inline, environment, `*_file`) — and the redacted summary still says `set`.
- A `Kind::Secret` key never appears in the settings payload's `values` at all.
- `*_file` keys are never written, however crafted the request. **New:** the save
  response reports which submitted keys it ignored, so the UI cannot say "Saved"
  about something that did not happen.
- Settings writes stay **page-scoped**. A `PUT` to one page cannot write another
  page's key — which is what keeps a single mis-scoped request from setting
  `policy.allow_hardlink`.
- A repeated section's row count cannot change across a `PUT`. Removal stays
  index-addressed with a server-computed orphan count, because a client that
  holds the rows as an array and deletes index 0 would otherwise write old-row-1
  into index 0 — a silent tracker **rename** that orphans every job filed under
  the old id.
- `""` means what `EMPTY_MEANS_ABSENT` says it means, absent means "not
  submitted", and `null` is a 400 — so removal never has two spellings.
- A submitted value is a **string**. `parse_field` stays the only thing that
  decides whether it is valid, so every error keeps its field attribution.
- `metrics.enabled` is not written on a build without the `metrics` feature. The
  hidden-input trick that guaranteed this in HTML has no JSON equivalent, so the
  refusal moves server-side.
- Candidate selection is by index into the server's own list. A path from the
  request is never used.
- The save pipeline validates before writing. The file on disk never becomes a
  configuration the process would refuse.
- One runtime generation per request, with **one documented exception**: the SSE
  stream re-reads `current()` per emit, because a stream that captured one
  generation would serve a staging root and tracker health from adapters replaced
  hours ago.
- **The SSE stream re-checks the session on every emit.** `require_auth_token`
  runs once per request and an event stream is one request; without the recheck,
  rotating the auth token leaves every open tab with a live feed of job names,
  staging paths and tracker errors on a session that was revoked.
- The SPA shell and its assets are served without a credential; everything under
  `/api/v1` requires one. This is a deliberate posture change from 0018 — a
  guarded shell gives a redirect loop and a blank page with no way in — and it is
  safe because the bundle contains no operator data. It is stated in
  `require_auth_token`'s doc comment and in the README.
- A request under `/api/` without credentials is 401 JSON, never a redirect.
- A non-JSON content type is refused. That is what makes "a cross-origin JSON
  fetch preflights and is blocked" true; an HTML form can post `text/plain`
  cross-site with no preflight.
- A cookie-authenticated write carries a CSRF token, and the "neither
  `Sec-Fetch-Site` nor `Origin`" allowance no longer applies to it.
- `/api/v1` returns JSON for an unknown path and an unknown method. The SPA
  history fallback never swallows `/health`, `/metrics`, or a request for an
  asset that does not exist.

## Likely files

`src/web/api/` (new), `src/web/spa.rs` (new), `src/events.rs` (new),
`src/web/mod.rs`, `src/web/settings/{mod,save}.rs`, `src/repair/ports.rs`,
`src/repair/adapters/sqlite.rs`, `src/repair/worker.rs`, `src/bootstrap.rs`,
`src/runtime.rs`, `src/config/mod.rs` (one field, one grep test),
`migrations/0007_job_query_indices.sql`, `examples/fixture.rs` (new), `web/`
(new), `Dockerfile`, `.dockerignore`, `.github/workflows/ci.yml`.

Deleted: `src/web/layout.rs`, `src/web/settings/render.rs`, and the maud halves of
`jobs.rs`, `status.rs`, `review.rs`, `login.rs` and `error.rs`. `maud` leaves
`Cargo.toml` — Vite emits an `index.html` with the asset tags already injected, so
keeping a template engine for fifteen lines of static HTML would be one
dependency for nothing.

## Required tests

Beyond the invariants above, each of which needs one:

- The generated TypeScript unions match the Rust enums, byte for byte.
- No TypeScript source contains `dangerouslySetInnerHTML`, `innerHTML`, `eval`,
  `new Function`, or web storage of a credential. A Rust grep test, so it runs in
  the existing `check` job with no Node — and it matters because this UI renders
  torrent names from a private tracker, filesystem paths, arbitrary audit-trail
  JSON, and `ProbeResult::detail` from whatever host the operator typed in.
- `counts_by_state` agrees with folding over every job in Rust.
- A keyset page neither repeats nor skips a row while rows are being updated.
- A search term containing `%` is matched literally.
- An out-of-range `candidate_index` is refused, and a traversal `torrent_path` is
  refused. **There is no HTTP-level test for `choose-candidate` today at all.**
- A bool submitted as `"false"` is written; the same key omitted leaves the
  document unchanged. (These are the properties `last_value_wins` and the
  hidden-input convention carried; both functions become unreachable and their
  properties must not go with them.)
- An open event stream survives a config reload and is told about it.
- The shipped image serves the real bundle, not the "UI was not built" notice.
- `cargo test` passes with no bundle built, and with one.

Browser tests own what only a browser knows: that the bundle boots, that a form
produces the right dotted keys, that no route overflows horizontally at 320px,
that touch targets clear 44px, that contrast and focus hold in both colour
schemes, that the read-only-config notice appears **before** the operator types,
that a validation error does not lose typing, and that a live update does not move
a row out from under a thumb — which matters because one of the buttons on that
row is "Abandon and discard staged files".

`TestClock` does not exist in a real process, so anything time-dependent —
`STUCK_TIME_THRESHOLD`, `recheck_timeout` parking, retry backoff, health
staleness — stays in Rust.

## Acceptance criteria

- Cold start to a staged repair entirely through the browser, proven twice: once
  over the JSON API against durable state, once in a real browser.
- No sentinel secret in any response from any route.
- Every destination reachable by clicking, from `/`, at 320px.
- Zero horizontal overflow and zero serious or critical accessibility violations
  on every route, in both colour schemes.
- A repair advancing is visible without a reload; disconnecting is visible too.
- `cargo build --locked` with no Node installed produces a binary that serves a
  page explaining how to build the UI.
- The runtime image still copies exactly three files and installs zero packages.

## What is not done yet

The settings pages are **still server-rendered `maud`**, reachable at
`/settings` and linked from the SPA's navigation. Everything else — the
dashboard, the repair list, the review queue, job detail, the candidate flow and
diagnostics — is the React client.

That split is deliberate rather than accidental. The read-and-act surface is
where the complaints were (no navigation, unusable on a phone, no live updates,
no feedback on an action), and it is now done and verified. The settings surface
is 49 fields across nine pages behind the most safety-critical code in the
repository, and porting it well is its own piece of work.

The groundwork for it is in place and cost nothing extra: `apply_pairs` already
takes a `Vec<(String, String)>`, so the JSON endpoint is an adapter that builds
that pair list — **no change to the six-step pipeline at all**, which is better
than this document's original plan of retyping `parse_field`. What remains is
`GET /api/v1/settings` (the `FIELDS` schema, current values, and secret
*sources*), `PUT /api/v1/settings/{slug}`, the row add/remove endpoints, and the
four shapes of settings page in React.

## Out of scope

- **A sub-path deployment beyond `server.base_path`.** The one new field covers a
  reverse proxy at a prefix; per-route rewriting and proxy-header trust do not
  follow from it.
- Accounts, roles, or per-user preferences. 0011's resolution stands: one shared
  secret, no login system.
- Offline or PWA behaviour. An operator UI that serves cached repair state while
  offline is lying about the present, which is priority 2 of the safety posture.
  The honest behaviour is "showing data from 14:02", which needs no service
  worker.
- Web push. The webhook notifier already reaches every notification service worth
  reaching, and a channel that only works while a tab is open is worse than one
  that works.
- Charts or throughput history. Nothing durable records a time series, so any
  chart would be invented from live polling. `/metrics` exists for a real scraper.
- Editing the file plan, or typing a source path. Out of scope in 0010, and a
  path from the request is hostile input by construction.
- Bulk approve-resume. Deliberately refused; 0010 left it as a resolved open
  question and the answer was no.
- Undo. There is no undo in the domain, so a UI undo would be the most direct
  possible violation of "never claim something happened that did not".
- Visual-regression snapshots. Font rendering differs between a developer's
  machine and CI, and this repository has no tolerance for a flaky test.

## Open questions

Resolved before writing code, recorded here:

1. **React or Preact?** React. Preact plus `preact/compat` is the same authoring
   API at roughly a tenth of the bundle, which is a real consideration when the
   bundle ships inside the binary — but the Radix primitives this UI leans on for
   focus management and listbox keyboard navigation are tested against React, and
   an operator UI's bundle size is not a constraint on a LAN. Revisit if the
   bundle passes 300 KB gzipped.
2. **Commit the built bundle, or build it in CI?** Build it in CI. Committing
   `web/dist` would keep `cargo build` alone producing a UI and would keep the
   native-arm64 image build free of a Node stage and of esbuild's per-platform
   binary — a genuine reproducibility argument. It loses to keeping generated
   artefacts out of git history and reviews. The cost is accepted and named: the
   arm64 job gains a Node stage, and a tracked placeholder `web/dist/index.html`
   is what keeps the build compiling for someone who only has cargo.
3. **Embed the bundle, or serve it from disk?** Embed, with `rust-embed`. Serving
   from disk needs a fourth `COPY` in a runtime image that deliberately has three,
   introduces a path question next to a `WORKDIR /` that is already load-bearing,
   and brings a path-traversal surface. `rust-embed` over `include_dir` for two
   things that would otherwise be hand-written and wrong: the `rerun-if-changed`
   without which `npm run build && cargo build` silently serves the previous
   bundle, and the per-file hash that *is* the ETag.
4. **Does `/login` stay a server-rendered page?** No. It becomes a client route,
   which is what forces the shell to be served unauthenticated — see the
   invariant above. Keeping a maud login page as a no-JavaScript escape hatch
   would mean two login UIs and keeping `maud` forever.
5. **Typed JSON bodies for settings, or dotted keys and string values?** Dotted
   keys and strings. Typed values move `Kind::Count`'s minimum check and its
   "`abc` is not a whole number" attribution into serde, turning an operator's
   typo into a body-level 400 the UI cannot paint next to a field. A typed partial
   `Config` would be worse: deserialising and writing it back is exactly the
   `Serialize`-of-`Config` path `ConfigDocument` exists to prevent.
6. **One settings endpoint or one per page?** One per page, as today. A single
   endpoint taking any key silently widens the write surface so that one
   mis-scoped request could set the one field `danger` exists for.
7. **Full payloads over SSE, or invalidation hints?** Hints. A moved job needs
   the list shape on one screen and the detail shape on another, so full payloads
   mean extra queries on every transition for clients that may not be looking,
   and would have to be authorised per subscriber — whereas a hint sends the
   refetch through the ordinary authenticated `GET`, so authorisation keeps one
   gate.
8. **Where does the event bus live?** On `Persistent`, beside `WorkerHealth` and
   `Diagnostics`, for the same reason those are there: the most interesting moment
   to be watching the dashboard is immediately after changing a setting, and a
   channel on `Runtime` would drop every subscriber on every save.
