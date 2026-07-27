# 0007 — qBittorrent WebUI adapter

**Status:** Done
**Depends on:** nothing
**Blocks:** 0008

## Problem

`seeding::adapters::qbittorrent::QBittorrentClient` fails every call, so nothing
is ever really injected, rechecked, or resumed. This and 0003 are the two pieces
that stand between the bootstrap and a system that does real work.

The qBittorrent WebUI API is stateful in an awkward way — cookie-based login that
expires — and reports about a dozen torrent states that have to collapse onto the
five the port models. Getting that mapping wrong is a safety issue: mistaking
`stalledDL` for a seeding torrent, or `checkingResumeData` for a finished check,
would let a repair advance on a false premise.

## Architectural context

`seeding::TorrentClient` models five operations and five states. The port
requires every method to be idempotent, because the workflow replays steps after
a crash. `add_paused` on a torrent that is already present is success, not an
error — the crash-recovery tests depend on it.

The capability is called `seeding` rather than `qbittorrent` deliberately;
qBittorrent is one adapter behind the port, and nothing outside this file may
assume it.

## Expected behaviour

- `add_paused` uploads the `.torrent` with the staging save path, in a paused
  state, in the configured category. Re-adding an existing torrent is a no-op
  that returns `Ok(())`.
- `status` returns `None` for an unknown info-hash, and otherwise the mapped
  state, the completeness, and the save path qBittorrent actually has — which
  may not be the one we asked for.
- `recheck` triggers a hash check. Re-issuing one that is already running is a
  no-op.
- `resume` starts the torrent. Resuming an already-started torrent is a no-op.
- `remove` never deletes files. The `delete_files` parameter exists but the
  workflow never passes `true`, and the adapter should refuse if it ever does —
  see the safety constraints.
- An expired session is detected and re-authenticated once, transparently.

## Implementation steps

1. **Session handling.** `POST /api/v2/auth/login` with username and password
   returns an `SID` cookie. Hold it behind a `tokio::sync::Mutex<Option<Cookie>>`
   or a `reqwest` cookie store. On a `403`, log in once and retry the request
   exactly once — never loop.

2. **`add_paused`.** `POST /api/v2/torrents/add`, multipart, with the `.torrent`
   bytes, `savepath`, `category`, `paused=true`, and `skip_checking=false`.
   Note the version split: newer qBittorrent renamed `paused` to `stopped`.
   Detect the version at login via `/api/v2/app/version` and send the right
   field, or send both. Record which approach you chose.

   The endpoint returns `Ok` even when the torrent already exists, but confirm
   that empirically. If it does not, `status` first and short-circuit.

3. **State mapping.** `/api/v2/torrents/info?hashes=<hash>` gives a `state`
   string. Map it exhaustively, with a `_ =>` arm that returns
   `ClientTorrentState::Errored` rather than guessing:

   | qBittorrent | Port |
   |---|---|
   | `pausedUP`, `pausedDL`, `stoppedUP`, `stoppedDL` | `Paused` |
   | `checkingUP`, `checkingDL`, `checkingResumeData`, `queuedForChecking`, `moving` | `Checking` |
   | `downloading`, `stalledDL`, `metaDL`, `queuedDL`, `forcedDL`, `allocating` | `Downloading` |
   | `uploading`, `stalledUP`, `queuedUP`, `forcedUP` | `Seeding` |
   | `error`, `missingFiles`, `unknown` | `Errored` |

   Verify this against the version you target; the list has changed over time.

4. **Completeness.** The `progress` field is `0.0`–`1.0`.
   `DataCompleteness::from_ratio` already treats anything below `1.0` as partial,
   which is the conservative reading — do not round.

5. **`recheck` and `resume`.** `POST /api/v2/torrents/recheck` and
   `/torrents/resume` (or `/start` on newer versions), both taking `hashes`.
   Both are already idempotent server-side; confirm and note it.

6. **`remove`.** `POST /api/v2/torrents/delete` with `deleteFiles`. Because
   staged data may be hardlinked to the library, hard-code `deleteFiles=false`
   and return an error if a caller passes `true` — the port keeps the parameter
   so the intent is explicit at the call site, but this adapter refuses to honour
   it. Document that in the adapter.

7. **Info-hash case.** qBittorrent uses lowercase hex. `InfoHash::to_hex`
   already produces lowercase; make sure comparisons stay case-insensitive
   anyway.

8. **Delete the stub** and the `const TODO`.

## Invariants and safety constraints

- **Never delete files through the client.** This is the rule that protects a
  hardlinked library file from a cleanup path.
- **Never add a torrent started.** There is no code path that adds without
  `paused`.
- An unmappable state is `Errored`, never a guess at something more optimistic.
- `progress < 1.0` is partial. No rounding, no epsilon.
- Password is `config::Secret` and never logged or included in an error.
- Re-authentication happens at most once per request. No retry loops.

## Likely files

- `src/seeding/adapters/qbittorrent.rs`
- `src/seeding/domain.rs` (only if a state is genuinely missing)
- `src/bootstrap.rs`, `src/config.rs`
- `Cargo.toml`

## Required tests

Wiremock:

- Login, then a request; on `403`, re-login and retry once, then succeed.
- A second `403` after re-login is an error, not another retry.
- `add_paused` sends `paused`/`stopped` true and the right save path.
- Adding an existing torrent returns `Ok(())`.
- `status` for an unknown hash returns `None`.
- Every documented state string maps as tabled; an unknown string maps to
  `Errored`.
- `progress: 0.999999` is `Partial`, `1.0` is `Complete`.
- `remove` with `delete_files = true` returns an error and issues no request.
- No test fixture or error message contains the password.

## Acceptance criteria

- Against a real qBittorrent, a repair reaches `seeding` with the torrent in the
  configured category and the correct save path.
- Killing SeedMedic between `add_paused` and the transition, then restarting,
  does not produce a duplicate torrent.
- Removing the torrent by hand causes the repair to rewind and re-add it.
- The stub and its `NotImplemented` are gone.

## Out of scope

- Other clients — Transmission, Deluge, rTorrent. The port is deliberately
  client-shaped rather than qBittorrent-shaped; adding one is a new adapter and
  a new document.
- Managing categories, tags, or ratio limits.
- Reading qBittorrent's own view of which trackers a torrent belongs to.

## Open questions

- Which qBittorrent versions to support? The `paused`/`stopped` rename is the
  main split. Detecting at login and adapting is more code; requiring a minimum
  version is simpler and less friendly.
- Does `torrents/add` really succeed for an existing torrent on every supported
  version, or is a pre-check needed?
- Is there a reliable way to ask whether a recheck is *queued* rather than
  running? `queuedForChecking` suggests yes, and it matters for how long the
  `Rechecking` state waits.
- Should the adapter verify that the save path qBittorrent reports matches the
  one we asked for, and rewind if not? Somebody moving the torrent in the client
  would otherwise leave us verifying data we do not control.
