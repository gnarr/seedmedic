# 0015 — Start without a configuration file

**Status:** Done
**Depends on:** 0014
**Blocks:** 0016

## Problem

SeedMedic cannot start without a hand-written TOML file.

`Config::load` reads `SEEDMEDIC_CONFIG` or `./config.toml` and returns
`ConfigError::Read` when the file is absent, which propagates out of `main` and
exits non-zero. So the first run of a container built from this repository —
`ENV SEEDMEDIC_CONFIG=/config/config.toml`, with `/config` an empty volume — is
`cannot read /config/config.toml: No such file or directory (os error 2)` and
nothing else. There is no page, no log line saying what to do, and no process
left running to say it to.

Falling back to defaults would not help either, because `Config::default()` is
itself rejected: `staging.root` defaults to an empty `PathBuf` and `trackers` to
an empty `Vec`, and both are hard errors.

The documented first run is therefore `cp config.example.toml config.toml` and
then editing two absolute paths in a 170-line file. That is the single largest
obstacle between SeedMedic and somebody running it.

## Architectural context

The root `AGENTS.md` already has the pattern this document needs:

> **Placeholders fail loudly.** An unimplemented adapter returns
> `NotImplemented { adapter, todo }` and the repair parks for review. It never
> returns an empty list, a default, or `Ok(())`.

An unset setting is the same situation as an unimplemented adapter: the workflow
cannot proceed, and the honest response is to park the repair for review with a
reason naming what is missing — not to guess, and not to refuse to boot.

That reframing is what makes this cheap. There is no second process mode, no
second router, and no unconfigured→configured transition to design. There is one
code path in which some adapters happen to be ones that always fail.

The alternative — a first-class "setup mode" that serves only settings pages —
was rejected. It costs a second router shape, a second meaning for `/health`, an
answer for what `/` and `/status` show, and a transition that has to build the
store, spawn the worker and reconcile, i.e. all of 0016 from a different starting
point. Its only benefit is not touching SQLite before the operator says where to
put it, and `data/seedmedic.db` is already the documented default.

## Expected behaviour

- A missing configuration file is a warning naming the path examined and the
  settings URL, not an error. The process starts.
- A configuration file that exists but does not parse stays a hard error. A typo
  in a safety setting must never be silently replaced by defaults.
- `Config::default()` is startable: no `Error`-severity problems.
- The worker runs. It ticks, records health, and claims nothing, because with no
  trackers there is nothing to discover.
- `/health` returns 200. The process genuinely is ready; it is idle. A readiness
  probe on a fresh container must pass, or any deployment with a healthcheck
  restart-loops before the operator can reach a page.
- `/` and `/status` render normally. `/status` already says "No trackers
  configured."
- Nothing is created on disk beyond the database: with no `staging.root` there is
  no `StagingRoot`, so nothing calls `create_dir_all`.
- A repair that reaches a step needing an unset setting parks for review with a
  reason naming the setting.

## Implementation steps

1. **`Config::load`** distinguishes three outcomes, where today there are two:
   file absent → `Config::default()` and

   ```rust
   warn!(path = %path.display(), settings = %settings_url,
         "no configuration file found; starting unconfigured");
   ```

   Unreadable-for-another-reason and unparseable stay `ConfigError::Read` /
   `ConfigError::Parse`. A `SEEDMEDIC_CONFIG` that is set but missing is also
   only a warning — that is exactly the Docker first-run case, and it is the
   deciding scenario for this whole document. Log the absolute path, so an
   operator whose bind mount silently failed can see that SeedMedic is looking
   somewhere other than where their file is.

   `--check-config` keeps hard-failing on a missing file (0014).

2. **`staging.root` unset becomes a state, not an error.** Remove the
   `staging.root is required` check from `problems()`; `problems_on_disk()`
   already skips its filesystem checks when the root is empty. Add a `Warning`
   naming the key so it appears in `--check-config` and, later, at the top of the
   settings page.

3. **`UnconfiguredStaging`**, a new adapter under `src/staging/adapters/`.
   `StagingFilesystem` has five fallible async methods — `free_bytes`,
   `materialize`, `inspect`, `discard`, `usage` — and each returns

   ```rust
   Err(StagingError::NotImplemented(NotImplemented {
       adapter: "staging",
       todo: "set staging.root — see Settings → Staging",
   }))
   ```

   **`root_path(&self) -> &Path` is the exception: it is synchronous and
   infallible**, so it cannot fail and must return something. Return an empty
   path, and make `/status` render "not configured" for an empty root rather than
   an empty string. That is the one place this adapter needs a caller to change.

   `bootstrap` wires it when `staging.root` is empty. A repair reaching the
   staging step then parks for review with that reason, which is the designed
   behaviour for a step that cannot proceed.

4. **`trackers` may be empty.** Downgrade the "at least one `[[trackers]]` entry
   is required" error to a warning. This is not a safety rule — zero trackers
   means discovery finds nothing and the worker idles, which is precisely what a
   fresh install is. Confirm `discover_hit_and_runs` over an empty tracker map is
   a no-op and does not log per tick.

5. **`download_client` becomes `Option<DownloadClientConfig>`.** This is what
   lets 0014 make empty qBittorrent credentials a hard `Error` without breaking
   `Config::default()`: absent means not configured, present means it must be
   complete. Wire an `UnconfiguredClient` adapter, same shape as step 3, with
   `todo: "set download_client — see Settings → Download client"`.

   `Option` threading touches `bootstrap::build_client`, the example config, and
   the docs. **If this document runs long, this is the step to cut:** keep
   `DownloadClientConfig` required with today's defaults and make missing
   credentials a `Warning` instead. The cost of cutting it is a worse failure
   message — a 401 from a URL nobody set, rather than "download_client is not
   configured" — for about sixty fewer lines. `UnconfiguredStaging` is not
   optional either way; it is what makes "no config at all" work.

6. **Loud warning when no auth token is set**, at startup and after every
   reload, naming the settings URL and saying plainly that anyone who can reach
   the port can change where SeedMedic writes.

7. **A setup banner in the page shell.** `layout::page` has no access to state
   today, and `web::error` renders a page with no state available at all, so
   give it a `Chrome` argument: `layout::page(chrome, title, body)`, with
   `Chrome::none()` for the error page and the real one everywhere else. About
   ten mechanical call sites, no magic, no task-locals. The banner lists the
   unmet settings from `problems()` and links to `/settings`, which does not
   exist until 0017 — until then the banner names the keys and the config file
   path, which is already better than today.

8. **Docs.** `README.md`'s two "Try it" blocks lose `cp config.example.toml
   config.toml`. `config.example.toml` notes that `staging.root` and
   `download_client` may be absent. `Dockerfile` notes that
   `SEEDMEDIC_CONFIG` may legitimately point at a file that does not exist yet.

## Invariants and safety constraints

- **A malformed configuration file is never replaced by defaults.** Only an
  absent file is. Otherwise one typo in a working deployment silently reverts
  every safety setting to its default, which is precisely the failure
  `deny_unknown_fields` exists to prevent.
- **No staging directory is created for an unset `staging.root`.**
  `StagingRoot::new` calls `create_dir_all`; it must not be reached with an empty
  or guessed path.
- **No default is invented for `staging.root`.** A guessed path would be
  silently accepted, giving a working-but-wrong staging area on the wrong
  filesystem while `/health` reports `ok` — a violation of the second priority,
  "never claim something happened that did not". Rejected candidates, for the
  record: a relative default breaks the absolute-path invariant that the overlap
  check's reasoning depends on; a cwd-derived default is surprising; and
  `/var/lib/seedmedic/staging` fails the writability check for a non-root user,
  which puts you straight back to a hard failure.
- **An unconfigured adapter never succeeds.** No empty list, no default, no
  `Ok(())`.
- `/health` must keep meaning "the database is reachable and the worker has
  ticked recently". Being unconfigured does not change either fact.

## Likely files

- `src/config.rs`
- `src/staging/adapters/unconfigured.rs` (new), `src/staging/adapters/mod.rs`
- `src/seeding/adapters/unconfigured.rs` (new), `src/seeding/adapters/mod.rs`
- `src/bootstrap.rs`
- `src/web/layout.rs`, `src/web/error.rs`, and every page module (the `Chrome`
  argument)
- `README.md`, `config.example.toml`, `Dockerfile`

## Required tests

- `the_default_configuration_is_startable` — `Config::default()` has no
  `Error`-severity problems. The premise of this whole document deserves its own
  named test.
- A missing file yields defaults plus a warning; an unparseable file still
  errors; a file that exists but is unreadable for another reason still errors.
- Every `UnconfiguredStaging` method returns an error whose message names
  `staging.root`. Same for `UnconfiguredClient`.
- Integration, `tests/unconfigured_start.rs`, over a real store built from
  `Config::default()`: the worker ticks; `/health` is 200; `/` and `/status`
  render and report no trackers configured; a job driven to `matched` parks for
  review with a reason naming `staging.root`; nothing was created under the
  temp directory.
- Discovery over an empty tracker map is a no-op and logs nothing per tick.

## Acceptance criteria

- `docker run` with an empty config volume comes up, serves a page, and says in
  both the log and the UI what needs setting — instead of exiting 1.
- `cargo run` in a directory with no `config.toml` does the same.
- A one-character typo in an existing `config.toml` still refuses to start.
- No directory is created anywhere because SeedMedic guessed a path.

## Out of scope

- Editing settings (0017). This document only makes an unconfigured process
  start and explain itself.
- Applying a configuration change without a restart (0016).
- Any change to what `/health` means.

## Open questions

- Should `/health` return 503 while unconfigured, since no repair can happen?

  **Resolved:** no, 200. `/health` is documented as "the database is reachable
  and the worker has ticked recently", and both are true — the worker is idle,
  not broken. Returning 503 would restart-loop any container with a healthcheck
  and prevent the operator from ever reaching the settings page, which is the
  opposite of this document's purpose. The unconfigured state is surfaced on the
  pages and in the log, where a human will see it, not in a probe a machine acts
  on.

- Should an unconfigured process refuse to open SQLite until the operator
  confirms `database.path`?

  **Resolved:** no. `data/seedmedic.db` is already the documented default, the
  path is restart-required anyway, and refusing to open it would mean `/` and
  `/status` cannot render — which forces the second router shape this document
  exists to avoid.

- Should `trackers = []` be a warning or stay an error?

  **Resolved:** a warning. It was never a safety rule, and it is the correct
  state of a fresh install. Note the pre-existing asymmetry it exposes: zero
  *candidate sources* is legal today and silently parks every repair, which is
  strictly worse and unflagged — 0014 makes that a warning too.
