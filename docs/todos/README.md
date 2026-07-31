# Implementation roadmap

Each document is a cohesive change: substantial enough to be worth doing,
small enough to review on its own. They are numbered in dependency order, but
several are independent of each other and can be done in parallel.

Read `AGENTS.md` first, then the document, then its "open questions" — resolve
those before writing code and record the answers in the document.

| # | Title | Status | Depends on |
|---|---|---|---|
| [0001](0001-worker-hardening.md) | Worker loop hardening and deeper reconciliation | Done | — |
| [0002](0002-unit3d-tracker.md) | Unit3D tracker adapter | Done | — |
| [0003](0003-torrent-parsing.md) | Bencode decoding and info-hash derivation | Done | — |
| [0004](0004-arr-candidate-discovery.md) | Sonarr and Radarr candidate discovery | Done | 0003 |
| [0005](0005-media-matching.md) | Piece verification and matching confidence | Done | 0003, 0004 |
| [0006](0006-staging-materialization.md) | Reflinks, cross-device handling, free space | Done | 0003 |
| [0007](0007-qbittorrent-adapter.md) | qBittorrent WebUI adapter | Done | — |
| [0008](0008-recheck-and-resume.md) | Recheck monitoring and safe-resume enforcement | Done | 0006, 0007 |
| [0009](0009-tracker-confirmation.md) | Tracker-side completion and seed monitoring | Done | 0002, 0008 |
| [0010](0010-manual-review.md) | Manual-review workflows | Done | 0005, 0008 |
| [0011](0011-configuration-and-secrets.md) | Configuration, secrets, and startup validation | Done | — |
| [0012](0012-observability.md) | Structured logging, metrics, and diagnostics | Done | 0001 |
| [0013](0013-end-to-end-testing.md) | End-to-end and fault-injection test harness | Done | 0008, 0009 |
| [0014](0014-configuration-problems-as-data.md) | Configuration problems as data | Done | — |
| [0015](0015-start-without-a-configuration-file.md) | Start without a configuration file | Done | 0014 |
| [0016](0016-a-swappable-runtime.md) | A swappable runtime | Done | 0015 |
| [0017](0017-the-settings-pages.md) | The settings pages | Done | 0014, 0016 |
| [0018](0018-browser-usable-authentication.md) | Browser-usable authentication | Done | 0016 |
| [0019](0019-connection-tests.md) | Connection tests | Done | 0017 |
| [0020](0020-a-container-that-just-runs.md) | A container that just runs | Done | 0015, 0017 |
| [0021](0021-a-react-operator-ui.md) | A React operator UI | In progress | 0016, 0017, 0018, 0019 |

## Suggested order

0001–0013 are done: the workflow, the adapters, and everything needed to repair a
real hit-and-run.

0014–0019 are one cohesive piece of work — **SeedMedic should be startable and
configurable without hand-editing TOML** — split so that each document ships
something on its own:

**0014** is a pure `src/config.rs` refactor and improves `--check-config`
immediately: it stops hiding the second mistake behind the first.

**0015** delivers the headline requirement by itself. A missing configuration file
becomes a warning instead of a fatal error, unset settings get adapters that fail
loudly and park a repair for review naming the setting, and a fresh container
comes up and explains itself instead of exiting 1. No UI needed.

**0016** is the machine 0017 needs — a runtime that can be rebuilt in place — and
is provable without any HTML.

**0017** is the settings UI. **0018** must land before it ships, or saving an auth
token locks the operator out of their own browser. **0019** adds the "Test
connection" buttons, which is what turns "nothing is happening" into an answer.

**0020** finishes the same thought from the other end. Configuring in a browser
is no use if getting to the browser needs a terminal: `docker compose up -d`
wanted a `mkdir`, a `chown` and an image that was never published. It also fixes
the one packaging mistake that could have damaged a library — a recursive chown
over a staging area whose files may be hard links into it.

**0021** rebuilds the operator UI as a React client over a JSON API, because two
requirements cannot be met inside 0017's constraint at any amount of effort: a UI
usable on a phone, and a UI that shows a repair progressing without a manual
reload. It is the largest change since 0017 and the only one that has ever made
the codebase bigger; its cost accounting says so plainly.

Amendments: 0016, 0017 and 0019 each overturn a resolved open question in
[0011](0011-configuration-and-secrets.md) — config reloading, configuring through
the web UI, and `--check-connections` respectively. 0021 overturns
[0017](0017-the-settings-pages.md)'s "no JavaScript, no asset pipeline, no API
surface" in the same way. Each amends the earlier document in place with the new
reasoning rather than contradicting it silently.

## Marking one done

Update the status line in the document and the row in this table. Delete the
`NotImplemented` stub it replaced, remove the `const TODO` that pointed at it,
and check whether the root `AGENTS.md` or `docs/architecture.md` now says
something untrue.
