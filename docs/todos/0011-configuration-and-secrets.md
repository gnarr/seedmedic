# 0011 — Configuration, secrets, and startup validation

**Status:** Not started
**Depends on:** nothing
**Blocks:** nothing

## Problem

Configuration works but is not deployable-grade:

1. **Secrets only come from the file.** A Docker or Kubernetes deployment wants
   `SEEDMEDIC_TRACKER_AITHER_API_KEY` or `api_key_file = "/run/secrets/aither"`.
   Today the only option is a plaintext key in `config.toml`, which then has to
   be mounted, backed up, and kept out of version control by hand.

2. **Validation is shallow.** `Config::validate` catches the cheap mistakes.
   It does not notice a tracker with no API key, a qBittorrent URL that does not
   resolve, a library root that does not exist, or a staging root on a
   filesystem with no free space. Every one of those becomes a confusing runtime
   failure hours later instead of a clear message at startup.

3. **There is no way to check a config without running.** An operator editing
   policy has to restart the service and watch the logs.

4. **The UI is unauthenticated and nothing says so.** It exposes library paths
   and can discard staged data. That may be fine on a home network; it needs to
   be a stated decision rather than an omission.

## Architectural context

`src/config.rs` parses one TOML file with `deny_unknown_fields`, validates it,
and converts it into `SafetyPolicy` and `WorkerConfig`. `bootstrap.rs` is the
only place that turns configuration into adapters.

The principle in the root `AGENTS.md`: anything that would make SeedMedic unsafe
or useless is rejected at startup, not defended against at every call site. This
document extends that reach.

## Expected behaviour

- Every secret can come from the file, an environment variable, or a file path,
  with a documented precedence.
- Startup fails with a specific, actionable message for a configuration that
  cannot work.
- `seedmedic --check-config` validates and exits, without touching the database
  or any network.
- The security posture of the web UI is documented, and there is at least a
  minimal way to protect it.

## Implementation steps

1. **Secret sources.** For each secret, accept `x`, `x_file`, and
   `SEEDMEDIC_<SCOPE>_<NAME>`. Precedence: environment, then `_file`, then
   inline; document it in `config.example.toml`. Trim trailing newlines from file
   contents — a `_file` secret written by `echo` is the classic footgun.

2. **Deeper validation**, all in `Config::validate` or a new `validate_runtime`
   that is allowed to touch the filesystem:
   - Every tracker of a kind that needs credentials has them.
   - Library roots exist and are readable directories.
   - The staging root's parent is writable.
   - `min_match_confidence = "exact"` warns while 0005 is unimplemented, because
     it silently sends every repair to review.
   - Intervals are sane: a `tracker_poll_seconds` of 1 will get somebody banned.
   - `worker.owner` is unique-looking if that ever matters (see 0001).

   Separate hard failures from warnings. A warning is logged loudly at startup;
   a failure refuses to start.

3. **`--check-config`.** Add `clap` — or, given there is exactly one flag, parse
   `std::env::args` by hand and skip the dependency. Validate, print a redacted
   summary of the effective configuration, exit non-zero on failure. The redacted
   summary is genuinely useful: "here is what I understood" catches more mistakes
   than any validation rule.

4. **Web UI protection.** Minimum: document in `README.md` and
   `config.example.toml` that the UI is unauthenticated and must not be exposed
   to the internet. Better: an optional `server.auth_token` checked as a header
   or cookie. Do not build user accounts.

5. **Config reloading.** Probably not worth it — restarting is cheap and the
   state is durable. Note the decision here rather than leaving it open.

## Invariants and safety constraints

- Secrets never appear in logs, error messages, the `--check-config` summary, or
  the web UI. `Secret`'s redacting `Debug` is the mechanism; anything that
  formats a secret another way is a bug.
- Validation may read the filesystem but must not write to it, contact the
  network, or open the database. `--check-config` has to be safe to run against
  a production config on a laptop.
- A configuration that could damage the library — a staging root inside a
  library root — must fail, not warn. That check already exists in
  `StagingRoot::new`; make sure `--check-config` reaches it.
- `deny_unknown_fields` stays. A typo in a safety setting must not be silently
  ignored.

## Likely files

- `src/config.rs`
- `src/bootstrap.rs`, `src/main.rs`
- `config.example.toml`, `README.md`
- `Dockerfile` (document the env vars)

## Required tests

- A secret resolves from environment over `_file` over inline.
- A `_file` secret with a trailing newline is trimmed.
- A missing `_file` is a clear error naming the path.
- A tracker needing credentials without any fails validation.
- A non-existent library root fails validation.
- A staging root inside a library root fails validation.
- `min_match_confidence = "exact"` warns but starts.
- No secret appears in the `--check-config` output — assert on the string.
- The example config still passes (the existing test).

## Acceptance criteria

- SeedMedic runs from a Docker Compose file with secrets in environment
  variables and no secret in `config.toml`.
- Every configuration mistake produces a message naming the key and what is
  wrong with it.
- `--check-config` is safe to run anywhere and prints a useful summary.
- The web UI's security posture is written down.

## Out of scope

- User accounts, roles, OIDC.
- Configuring through the web UI.
- Secret managers — Vault, SOPS. `_file` covers the mount-a-secret case, which
  is what these deployments do anyway.

## Open questions

- What is the environment-variable naming scheme for a per-instance secret?
  `SEEDMEDIC_TRACKER_<ID>_API_KEY` requires uppercasing an operator-chosen id,
  which can collide. An explicit `api_key_env = "..."` per instance is uglier
  and unambiguous.
- Is `clap` worth a dependency for one flag?
- Should `--check-config` verify connectivity to trackers and the client? Very
  useful, and it makes the command touch the network, which breaks the "safe to
  run anywhere" property. Perhaps a separate `--check-connections`.
- Optional auth token, or documentation only?
