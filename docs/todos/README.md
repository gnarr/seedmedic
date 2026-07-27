# Implementation roadmap

Each document is a cohesive change: substantial enough to be worth doing,
small enough to review on its own. They are numbered in dependency order, but
several are independent of each other and can be done in parallel.

Read `AGENTS.md` first, then the document, then its "open questions" — resolve
those before writing code and record the answers in the document.

| # | Title | Status | Depends on |
|---|---|---|---|
| [0001](0001-worker-hardening.md) | Worker loop hardening and deeper reconciliation | Not started | — |
| [0002](0002-unit3d-tracker.md) | Unit3D tracker adapter | Not started | — |
| [0003](0003-torrent-parsing.md) | Bencode decoding and info-hash derivation | Not started | — |
| [0004](0004-arr-candidate-discovery.md) | Sonarr and Radarr candidate discovery | Not started | 0003 |
| [0005](0005-media-matching.md) | Piece verification and matching confidence | Not started | 0003, 0004 |
| [0006](0006-staging-materialization.md) | Reflinks, cross-device handling, free space | Not started | 0003 |
| [0007](0007-qbittorrent-adapter.md) | qBittorrent WebUI adapter | Not started | — |
| [0008](0008-recheck-and-resume.md) | Recheck monitoring and safe-resume enforcement | Not started | 0006, 0007 |
| [0009](0009-tracker-confirmation.md) | Tracker-side completion and seed monitoring | Not started | 0002, 0008 |
| [0010](0010-manual-review.md) | Manual-review workflows | Not started | 0005, 0008 |
| [0011](0011-configuration-and-secrets.md) | Configuration, secrets, and startup validation | Not started | — |
| [0012](0012-observability.md) | Structured logging, metrics, and diagnostics | Not started | 0001 |
| [0013](0013-end-to-end-testing.md) | End-to-end and fault-injection test harness | Not started | 0008, 0009 |

## Suggested order

**First real integration.** 0003 then 0007: with bencode decoding and a working
qBittorrent adapter, everything except tracker communication is real, and the
fake tracker can still drive it.

**First useful deployment.** Add 0002 and 0009, and SeedMedic repairs real
hit-and-runs on one tracker family — with matching still limited to the
filesystem walk.

**Then quality.** 0004, 0005, and 0006 make matching and staging good rather
than merely correct. 0010 makes the review queue actionable. 0011, 0012, and
0013 make it operable.

## Marking one done

Update the status line in the document and the row in this table. Delete the
`NotImplemented` stub it replaced, remove the `const TODO` that pointed at it,
and check whether the root `AGENTS.md` or `docs/architecture.md` now says
something untrue.
