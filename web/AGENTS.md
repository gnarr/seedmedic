# AGENTS.md — `web`

The operator UI. Supplements the root `AGENTS.md` and `src/web/AGENTS.md`; it
never contradicts either. See `docs/todos/0021-a-react-operator-ui.md` for why
this exists at all, including the honest accounting of what it cost.

## This subtree renders. It decides nothing.

`src/web/AGENTS.md` says the web module contains no rules of its own, because "a
decision the UI could make differently from the worker is a decision in the wrong
place". That rule binds harder here, because this *is* a separate program that
could disagree. Concretely:

- **No client-side validation.** A settings value is valid when
  `Config::problems()` says so, and the only way to find out is to ask.
- **No re-deriving which actions are legal.** `GET /api/v1/jobs/{id}` returns an
  `actions` map with an `available` flag and a `why`; render that. Working it out
  from `state` puts the transition table in two languages.
- **No copies of server prose.** `ReviewReason::description`, the refusal text on
  a 409, and the per-field settings help all arrive over the wire. A string
  literal here that duplicates one there is a bug waiting for the Rust side to
  change.
- **Formatting is ours; rules are theirs.** Bytes, relative times and colour are
  presentation. Thresholds, ordering and legality are not.

## Never re-order a list the operator is interacting with

A live event may change what a row *says*; it may not change where the row *is*.
Mark the changed rows and offer an explicit "reorder" control instead — see
`screens/repairs.tsx`. One of the buttons on a repair is *Abandon and discard
staged files*, so a row moving under a thumb is a safety problem, not a polish
problem.

## Never claim a transition that has not happened

Optimistic updates are allowed about *interaction* — a panel may lock and say
what it is doing — and never about *facts*. The state chip moves when the server
says it moved. Showing `matched` before the compare-and-swap confirmed it is
exactly priority 2 of the root `AGENTS.md`'s safety posture: "never claim
something happened that did not."

There is no undo, because there is no undo in the domain. Confirm, then act.

## Never `dangerouslySetInnerHTML`, `innerHTML`, `eval`, or `new Function`

What this UI renders includes torrent names from a private tracker, filesystem
paths, arbitrary audit-trail JSON, and `ProbeResult::detail` — which the README
itself describes as coming from "an authenticated arbitrary-outbound-GET
primitive". React escapes by default; the only way to lose that is to ask for it.

The Content-Security-Policy served with the shell is
`script-src 'self'; style-src 'self'` with no inline anything, which is why the
theme is applied from `main.tsx` rather than an inline script in the shell.

## Never put a credential in web storage

The session cookie is `HttpOnly` on purpose. `localStorage` holds exactly one
thing — the theme preference — and must never hold a token, a session id, or any
part of a response body.

## Candidate selection is by index

`POST /jobs/{id}/choose-candidate` takes `{torrent_path, candidate_index}`, where
the index is into the list the *server* recorded when it parked the job. Never
send a path you constructed. A torrent-supplied path is hostile input, and the
index is what guarantees an operator can only pick something matching already
found and offered.

## The dependency budget

Runtime: `react`, `react-dom`. That is the whole list.

Everything else is hand-written: a ~40-line router over `history`, a `fetch`
wrapper, and the component inventory in `ui.tsx`. Dialogs are the native
`<dialog>` element, which gives focus trapping, Escape-to-close, `inert` on the
rest of the page and a `::backdrop` from the platform — the reasons a dialog
primitive is normally a dependency.

Adding a runtime dependency needs a line in this file saying which of "easier to
understand, safer to change, harder to misuse, cheaper to operate, cheaper to
maintain, easier to test, easier to delete" it buys. The root `AGENTS.md` asks
that of every abstraction; a package is one.

## Accessibility is asserted, not assumed

`e2e/quality.spec.ts` runs axe plus four hand-written sweeps — horizontal
overflow, target size, accessible names, heading structure — over every route at
every viewport. The old UI would have failed all four.

Two rules that are easy to lose:

- **Nothing is encoded by colour alone.** Every state chip carries a glyph and a
  word, and `completed`/`failed` differ in glyph *shape* so they survive
  greyscale and a red/green deficiency.
- **Every light/dark colour pair is contrast-checked by computation**, not by
  eye. Three of them failed 4.5:1 on the first pass and were darkened; the ratios
  are recorded in `styles.css`.

## What cannot be tested from here

`TestClock` does not exist in a real process, so anything time-dependent —
`STUCK_TIME_THRESHOLD`, `recheck_timeout` parking, retry backoff, health
staleness — stays in the Rust tests where the clock can be moved. Do not reach
for `page.waitForTimeout(3600000)`.

## Node is named in one place

`web/.nvmrc`. The Dockerfile's `ARG NODE_VERSION` and CI's `node-version-file`
both read from it, mirroring the discipline `ARG RUST_VERSION` / `RUST_VERSION` /
`rust-version` already follow.

## `web/dist` is generated

Built by `npm run build`, gitignored, and embedded into the binary at compile
time by `include_dir!`. The tracked `web/dist/.gitkeep` must survive: without the
directory, `cargo build` fails for anyone who has not run the front-end build —
which includes CI's `check` job, deliberately.
