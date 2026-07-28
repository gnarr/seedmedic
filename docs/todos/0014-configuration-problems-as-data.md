# 0014 — Configuration problems as data

**Status:** Not started
**Depends on:** nothing
**Blocks:** 0015, 0016, 0017

## Problem

`Config::validate` and `Config::validate_runtime` bail at the first thing they
find, as an untyped `ConfigError::Invalid(String)`. Three consequences:

1. **`--check-config` hides the second mistake behind the first.** An operator
   fixes a relative `staging.root`, re-runs, and discovers their tracker has no
   API key. Fix that, re-run, discover `tracker_poll_seconds` is below the
   floor. The command exists to catch mistakes in one pass and does not.
2. **No problem knows which key it is about.** `"policy.max_attempts must be at
   least 1"` names the key in prose only. A settings form (0017) needs to put a
   message next to a field, which means the key has to be data.
3. **Warnings and errors are shaped differently for no reason.** `validate`
   returns `Result<(), ConfigError>` and `validate_runtime` returns
   `Result<Vec<String>, ConfigError>`, so "everything wrong with this config"
   cannot be expressed as one value.

There is also a set of validation gaps that will become much more visible once a
UI can write the config, because a form makes every one of them a click away.

## Architectural context

`src/config.rs` is the only module that decides whether a configuration is
usable. The principle in the root `AGENTS.md` is that anything which would make
SeedMedic unsafe or useless is rejected at startup rather than defended against
at every call site; this document does not change that reach, only the shape of
the answer.

The existing two-tier split is load-bearing and stays: `validate` is cheap and
touches nothing, `validate_runtime` may read the filesystem but must never write
to it, contact the network, or open the database. That is what makes
`--check-config` safe to run against a production config from a laptop
(`0011`'s invariants).

`bootstrap::build` calls `config.validate()` a second time. That call site is
the reason the `Result`-returning wrapper has to survive this change: if
`validate` became `-> Vec<Problem>`, that line would silently become a no-op
with no help from the compiler.

## Expected behaviour

- One value describes everything wrong with a configuration, each item carrying
  the dotted TOML key it is about and whether it is fatal.
- `--check-config` prints every problem in one pass, grouped errors-then-
  warnings, and exits non-zero if there is at least one error.
- `--check-config` still fails hard when the file does not exist. It must never
  print "configuration OK" for a path that is not there.
- Startup behaviour is otherwise unchanged: an error refuses to start, a warning
  is logged loudly.

## Implementation steps

1. **The type.**

   ```rust
   #[derive(Clone, Copy, Debug, Eq, PartialEq)]
   pub enum Severity { Error, Warning }

   /// One thing wrong, attributed to the key that is wrong, so a settings form
   /// can put the message next to the field and `--check-config` can print all
   /// of them at once.
   #[derive(Clone, Debug, Eq, PartialEq)]
   pub struct Problem {
       /// Dotted key with concrete indices — `trackers.1.api_key`, not
       /// `trackers`. `None` only for a problem about the configuration as a
       /// whole.
       pub key: Option<String>,
       pub severity: Severity,
       pub message: String,
   }
   ```

2. **The two entry points**, replacing the bodies of `validate` and
   `validate_runtime` and keeping their split:

   ```rust
   impl Config {
       /// Every problem findable without I/O.
       pub fn problems(&self) -> Vec<Problem>;
       /// Every problem that needs the filesystem. Never writes, never touches
       /// the network, never opens the database.
       pub fn problems_on_disk(&self) -> Vec<Problem>;
   }
   ```

   Neither returns `Result`. A check that cannot be performed (an unreadable
   directory) is itself a `Problem`, not an error about checking.

3. **Keep `validate()` as a thin wrapper** returning
   `Result<(), ConfigError>`, whose `ConfigError::Invalid` message is every
   error joined by newlines. This keeps `bootstrap::build` correct and keeps the
   existing ~20 unit tests meaningful without rewriting them — the ones that
   assert on message content still pass, because the joined message contains
   each part. Exactly one test needs adjusting: the `min_match_confidence =
   "exact"` warning test, which currently inspects a `Vec<String>`.

4. **Tighten the existing tests to assert on `key`** rather than only
   `is_err()`. `assert!(parse(&cfg).is_err())` passing for the wrong reason is
   a real hazard in a file this size, and the key is the cheap fix.

5. **`--check-config`** prints the redacted summary, then the problems, then
   exits 1 if any error. Keep `main.rs` a printer; test `problems()` directly.
   Verify the missing-file case still hard-fails.

6. **Close the validation gaps.** Each becomes one `Problem`:

   | Gap | Severity | Why |
   |---|---|---|
   | `arr[].name` empty or duplicated | Error | It is the derived env-var key and the `CandidateOrigin` label written into the audit trail. Two instances named `main` give two indistinguishable audit rows and one env var feeding both. Factor one helper shared with the existing tracker-id check. |
   | Two tracker ids or arr names that collide only after `shouty()` | Error | `0011`'s resolved open question explicitly justified deriving env-var names on the grounds that a collision "is exactly the kind of confusing setup `validate` should reject on its own merits". It never did. Close the promise. |
   | `retry_max_seconds < retry_base_seconds` | Error | Exact precedent two lines away for the recheck pair. Inverted, the backoff clamps to the max and never grows — silently degrading, never visible. |
   | `download_client.kind = "qbittorrent"` with an empty username or password | Error | Today entirely unchecked, so a wrong or missing credential surfaces as a 401 on the first tick that needs it. **Requires `download_client` to become optional — see 0015**, otherwise `Config::default()` gains an error. |
   | `download_client.base_url` still the `http://localhost` placeholder | Warning | Distinguishable from "deliberately localhost" only by intent. |
   | Zero candidate sources — no `[[arr]]`, empty `library.roots` | Warning naming both keys | Not an error: it is the correct state of a fresh install. But it is the single most likely cause of "SeedMedic isn't doing anything", so it must be loud. |
   | `notifications.webhook_url` scheme not `http`/`https` | Error | `Url` happily accepts `file:` and `mailto:`. |
   | `server.auth_token` unset | Warning | New, and more consequential once the UI can rewrite the config (0017). |
   | `staging.min_free_bytes == 0` | Warning | Disables the margin the field exists for. |
   | `policy.verification_pieces == 0` | Warning | Silently caps confidence below `Exact`, which interacts with `min_match_confidence`. |
   | `recheck_timeout_seconds < recheck_poll_max_seconds` | Warning | You time out before the second poll. |
   | A `fake` tracker configured alongside a real one | Error | `bootstrap::build_inspector` returns the fake inspector only when *every* tracker is fake, so this combination silently makes the fake tracker's torrents fail to parse. Today it is a doc comment on a sharp edge; adding a real tracker next to the shipped demo one is the most likely first action of a new user, so it has to be a rejection. |
   | `kind = "fake"` in a build without the `fakes` feature | Error | Today an `anyhow::bail!` inside `build_trackers`/`build_client`. Moving it here means the message arrives from validation like every other configuration mistake. |

   Deliberately **not** added: a warning for an `http:` webhook to a non-loopback
   host. Judgement call, likely noise.

7. **Keep "library root is not a readable directory" an error.** It is a real
   problem, and after 0015 the process stays up and says so rather than exiting.

8. Fix the status table in `docs/todos/README.md`, which lists all thirteen
   existing documents as "Not started" while each document's own status line
   says Done. Do it here rather than later — leaving it wrong through six more
   documents is worse.

## Invariants and safety constraints

- `problems()` performs no I/O of any kind. `problems_on_disk()` may read the
  filesystem and must not write to it, contact the network, or open the
  database. A test asserts `problems()` reports nothing about a non-existent
  path, which is the cheap proof.
- A configuration that could damage the library — a staging root inside a
  library root — stays an `Error`. Never a warning.
- No problem message contains a secret value. The existing `Secret` redaction is
  the mechanism; problems talk about keys, never values.
- `deny_unknown_fields` stays.

## Likely files

- `src/config.rs`
- `src/main.rs`
- `docs/todos/README.md`

## Required tests

- Every existing `config.rs` test passes unchanged. That is itself the
  regression net for this rewrite, and it is the right shape.
- Three independent mistakes produce three problems, not one.
- Every problem carries a concrete dotted key — `trackers.1.api_key`, not
  `trackers`.
- `problems()` reports nothing about `/nonexistent/...`, proving it does no I/O.
- One test per gap closed in step 6, each asserting the key and the severity.
- A warning does not make `validate()` fail.
- `--check-config` on a config with two errors and one warning prints all three
  and exits non-zero.
- `--check-config` on a missing file still fails and does not print
  "configuration OK".

## Acceptance criteria

- One `--check-config` run reports every mistake in a broken configuration.
- Every problem names the key an operator has to go and change.
- No behavioural change at startup: the same configurations start, the same ones
  refuse.

## Out of scope

- Any web UI. This document is `src/config.rs` and `src/main.rs` only.
- Reloading configuration (0016).
- Connectivity checks (0019). `problems_on_disk()` stays network-free.

## Open questions

- Should `Problem::key` be a typed key rather than a `String`?

  **Resolved:** a `String`. The keys have to carry concrete array indices
  (`trackers.1.api_key`), so an enum would need a payload per repeated section
  and would buy nothing that a test asserting on the string does not already
  buy. 0017 matches these strings against its own field table, and a drift test
  there keeps the two honest.

- Should `validate()` be deleted in favour of callers inspecting the problem
  list?

  **Resolved:** no. `bootstrap::build` calls it, and a function returning
  `Vec<Problem>` that a caller ignores compiles silently. Keeping a
  `Result`-returning wrapper means the one place that must refuse to start
  cannot accidentally stop refusing.

- Should the severity of "library root unreadable" be relaxed to a warning, so
  an unmounted network share does not stop the worker?

  **Resolved:** no. It stays an error. The improvement that matters here comes
  from 0015 — the process stays up and shows the problem on a page instead of
  exiting — not from pretending a missing library is survivable. A repair whose
  candidate source has vanished should park for review, and it will.
