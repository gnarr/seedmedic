# 0020 — A container that just runs

**Status:** Done
**Depends on:** 0015, 0017
**Blocks:** nothing

## Problem

`docker compose up -d` did not work.

It needed `mkdir -p config data staging` first, then `chown -R 10001:10001` on
those directories, because the image baked an unprivileged uid in at build time
and Docker creates a missing bind-mount source as `root:root`. And after all
that it still failed, at the pull: `docker-compose.yml` named
`gnarr/seedmedic:latest`, and nothing in this repository has ever built, tagged
or pushed an image. The one workflow runs `fmt`, `clippy` and `test`.

0015 and 0017 removed the need for a configuration file entirely — a fresh
install comes up unconfigured, explains itself, and is finished in a browser.
What was left between an operator and that first page was host filesystem
preparation and a registry with nothing in it.

The fixed uid was not only an inconvenience. Staged files are handed to
qBittorrent, which in every real deployment runs as uid 1000; a media library
readable by its owner is not readable by 10001; and `config.toml` is meant to
stay hand-editable, which it is not when it is owned by a system uid the
operator has no account for.

## Architectural context

**The staging path is the only path SeedMedic hands to another program.**
`repair::application::inject` passes `staging.save_path(...)` straight to
`AddTorrent::save_path`, and `PathMappingConfig` exists only under `[[arr]]`.
Sonarr and Radarr disagreeing about where a file is is a solved problem;
qBittorrent disagreeing is not, and it fails late — staging succeeds, injection
succeeds, and the recheck returns 0% after all the work is done.

That asymmetry decides the layout, because the two mounts are not the same kind
of thing:

| | Who else must resolve the string? | Cost of disagreement |
|---|---|---|
| `staging.root` | the download client, verbatim | a repair that fails after doing all its work |
| `library.roots` | nobody — SeedMedic only reads it | none |

So staging is mounted at **the same path inside and outside the container**, and
the library is mounted at a **fixed `/srv/media`**. Each is shaped by the
constraint that actually applies to it, and the rule to teach is one sentence:
*the only path that must be the same everywhere is staging.*

**One directory for everything SeedMedic owns, with no Rust change.**
`DatabaseConfig::default()` is the *relative* `data/seedmedic.db`, resolved
against the process working directory. `WORKDIR /` plus
`SEEDMEDIC_CONFIG=/data/config.toml` therefore lands the database at
`/data/seedmedic.db`, beside the config, without touching a line of Rust or a
line of `config.example.toml`. That coincidence is invisible and load-bearing,
which is why `tests/packaging.rs` asserts it rather than leaving it to a comment.

**Privileges are dropped by the entrypoint, not by `USER`.** `setpriv` is
util-linux, which is `Essential: yes` in Debian — already in
`debian:bookworm-slim` and not removable — so this needs no `gosu`, no
`su-exec`, and no package install. It `execve`s in place, so `seedmedic` is
still PID 1 and `shutdown_signal()` still sees SIGTERM from `docker stop`.

Two things follow from dropping privileges *by number*:

- `--clear-groups`, not `--init-groups`. The latter needs an `/etc/passwd` entry
  for the uid, which a runtime-chosen `PUID` does not have. Not creating one is
  the point: nothing in the image is mutated, so this behaves identically under
  `--read-only`, and there is no `usermod`/`groupmod` collision handling to get
  wrong. `groupmod -o` will happily create a duplicate gid and `usermod -o -u 0`
  will happily make the user root; all of that class of bug is deleted by not
  going near it.
- The `seedmedic` passwd entry in the image is purely decorative, so `ls -l` and
  `ps` show a name in the default case. Nothing reads it.

## Expected behaviour

- `docker compose up -d` in a bare checkout, with no `.env`, no `mkdir` and no
  `chown`, brings up a healthy container.
- The host directories end up owned by `PUID`:`PGID`, from the `root:root` state
  Docker leaves behind.
- No configuration file is written. `/data/seedmedic.db` is the only file that
  appears; `config.toml` appears the first time the operator saves at
  `/settings`.
- An unconfigured container is `healthy`. Being unconfigured changes neither of
  the two facts `/health` reports.
- The startup log names the two paths to enter, once, only while `config.toml`
  is absent.
- `docker run <image> --check-config` and `docker run <image> seedmedic
  --check-config` both work.
- `user:` in compose, or `--user`, still starts — it simply skips the chown and
  the privilege drop.
- `docker stop` shuts down gracefully rather than being killed after the
  10-second grace period.
- The image is published for `linux/amd64` and `linux/arm64`, to both
  `ghcr.io/gnarr/seedmedic` and `gnarr/seedmedic`.

## Implementation steps

1. **`docker/entrypoint.sh`** — normalise `$@` so a leading `-*` gets
   `seedmedic` prepended; `exec` straight through when already non-root; default
   `PUID`/`PGID` to `1000:1000` and reject non-numeric values; take ownership;
   `exec setpriv`. Every branch is an explicit `if` or `|| { …; }` — relying on
   `set -e`'s exemption for a non-final AND-OR member is how these scripts rot.

2. **Ownership, guarded on the top-level owner**, so the pass happens on the
   first start and after a deliberate `PUID` change and never again. `/data`
   recursively — it is small and entirely ours. The staging root **top level
   only**; see the invariants below. The media mount never.

3. **A chown failure warns and continues.** Docker Desktop's virtiofs, CIFS and
   NFS with `root_squash` all refuse `chown` while being perfectly usable, and a
   read-only mount fails later with a message that names the real problem.
   Exiting here would replace a good error with a worse one.

4. **`Dockerfile`** — `ARG RUST_VERSION`; `COPY Cargo.lock`, not `Cargo.lock*`,
   because `--locked` means nothing if a missing lockfile may resolve a fresh
   one; cache-mount ids keyed by `TARGETARCH`, or a local two-platform build has
   both architectures thrashing one `target/` under `sharing=locked`;
   `WORKDIR /`; `HOME=/tmp`; the healthcheck; `ENTRYPOINT` plus
   `CMD ["seedmedic"]`.

5. **Delete `VOLUME`.** The old declaration named `/staging`, a path that was
   never created, never chowned and referenced by nothing — but every entry had
   the same defect in waiting: `VOLUME` makes a bare `docker run` mint a
   root-owned anonymous volume, which is exactly the case 0017 had to detect and
   refuse in the settings UI. Run-time ownership makes the declaration
   pointless, and dropping it also stops anonymous volumes accumulating.

6. **The healthcheck is bash's `/dev/tcp`.** `debian:bookworm-slim` has neither
   `curl` nor `wget`, and `apt-get install curl` pulls sixteen packages and
   +11 MB — including `libssl3`, into an image that deliberately contains no
   OpenSSL because `reqwest` is built on rustls and webpki-roots. `bash -c` *is*
   the shell, so `${SEEDMEDIC_HEALTH_PORT:-9899}` expands at container run time.

7. **`docker-compose.yml` and `.env.example`** — every variable has a default,
   so `.env` is optional. Copying it must not become the new `mkdir`.

8. **`.dockerignore`** — add `/config`, `/staging` and `/.env`. Running the
   compose file in a checkout and then `docker build .` uploaded the whole
   staging area, potentially terabytes, and the operator's secrets, into the
   build context.

9. **`.gitattributes`** — `*.sh text eol=lf`. A CRLF checkout yields
   `bad interpreter: /bin/sh^M`, which is a confusing way to discover a line
   ending.

10. **CI** — pin the toolchain to one number named in three files; an
    `image-smoke` job on pull requests; per-architecture builds pushed by digest
    on native runners; one `publish-image` job assembling both registries'
    manifest lists from those digests.

11. **The `staging.root` help text** in `src/web/settings/fields.rs` gains a
    sentence saying the download client must see the same path. This is the one
    place the operator is looking at the moment they choose the value, and it
    helps host installs and `/downloads`-remapping users as much as Docker.

12. **Amend 0017 step 4** in place, per the convention: its `VOLUME` and
    uid-10001 reasoning is now historical, though the in-place-write fallback it
    justifies is still needed for bind-mounted *files*.

## Invariants and safety constraints

- **The staging chown is never recursive, and never `-L`.**
  `staging::adapters::local` materialises by `std::fs::hard_link`, so a staged
  file is *the same inode* as the library file. `chown -R` over the staging root
  rewrites the owner of files inside the media library — a write to the library,
  which the first rule in `AGENTS.md` forbids unconditionally. Verified
  empirically, not reasoned about: a hardlinked file at `nlink=2` changed owner
  through the staging name. Owning the directory is sufficient, because the
  process creates, links and unlinks *inside* it and never writes into an
  existing staged file. `-L` is worse still: it follows symlinks out of the tree.

- **The media mount is read-only and the entrypoint never touches it.** Belt and
  braces over "the library is read-only".

- **No configuration file is seeded**, and this is the interesting one, because
  the argument for seeding is real: 0015 forbids the *application* from
  inventing a path on an unknown host, and says nothing about an *image*
  declaring the layout it created itself. It still loses, on two counts.

  Seeding `library.roots = ["/srv/media"]` makes an unmounted library either a
  hard startup failure — `problems_on_disk` treats an unreadable root as an
  `Error` — or, if the image ships an empty `/srv/media` to prevent that, a
  silent lie: no warning, `/health` green, and every repair parking with "no
  library file matches" while SeedMedic asserts it has a library it does not
  have. That is priority 2, "never claim something happened that did not".

  Seeding `staging.root` recreates 0015's own stated failure mode one layer out
  — "a working-but-wrong staging area while `/health` reports ok" is exactly
  what happens when SeedMedic's side is right and the operator never added the
  mount to their qBittorrent container. The unset state is not friction; it is
  the forcing function that gets the operator to `/settings`, which is the only
  place that can explain the qBittorrent requirement at the moment they act on
  it.

- **`/` is not writable by the runtime uid, and that is a feature.**
  `problems_on_disk` walks to the nearest existing ancestor and checks
  writability, so a mistyped staging root is refused inline by the settings page
  naming the path, rather than silently working somewhere useless. Do not
  pre-create a writable `/staging` in the image and throw that away.

- **Privileges are dropped before `seedmedic` runs, via `exec`**, so PID 1
  signal handling and the existing graceful shutdown still work.

- **The compose file mounts a directory at `/data`, never a single file.**
  `ConfigDocument::save`'s rename path needs it (0017 step 3).

- **The shipped `.env.example` defaults must not overlap.** Siblings, so
  `StagingRoot::check_overlap` passes, they share a filesystem for reflinks, and
  staging is not inside the read-only mount. One instruction gets all three
  right: make it a sibling of your media directory.

## Likely files

- `docker/entrypoint.sh`, `.env.example`, `.gitattributes`, `tests/packaging.rs`
  (all new)
- `Dockerfile`, `.dockerignore`, `docker-compose.yml`, `docker-compose.test.yml`
- `.github/workflows/ci.yml`, `Cargo.toml`
- `src/web/settings/fields.rs`
- `README.md`, `config.example.toml`
- `docs/todos/README.md`, `docs/todos/0017-the-settings-pages.md`

## Required tests

Nothing tested the image or the compose files, and a container harness would be
out of all proportion to a compose file and eighty lines of shell. Two tiers
instead, aimed at the invariants that fail *silently*.

`tests/packaging.rs` — plain Rust over the shipped file text, no Docker needed,
no new dependency:

- every `${VAR}` the compose file interpolates is documented in `.env.example`.
  An undefined variable resolves to an empty string, which corrupts a mount spec
  rather than failing.
- the staging mount is the same string on both sides. The whole design in one
  assertion, and the one a well-meaning cleanup would break.
- the shipped `MEDIA_PATH` and `STAGING_PATH` defaults do not overlap, through
  the real `StagingRoot::check_overlap` — shipping defaults that hard-fail every
  new install is a live risk.
- the container's config and database share one directory, by parsing `WORKDIR`
  and `ENV SEEDMEDIC_CONFIG` out of the `Dockerfile` and joining
  `Config::default().database.path` onto the workdir. This is the invisible
  coincidence the layout rests on; change the default and the container splits
  its state across two directories, only one of which is mounted, with nothing
  else noticing.
- the Dockerfile declares no `VOLUME`.
- the entrypoint's staging chown passes an empty recursion flag — the safety
  rule above, asserted rather than trusted to review.

`image-smoke` in CI — the only thing that executes the entrypoint at all.
Without it a CRLF line ending, a lost `+x` bit, a `set -e` regression or a
`setpriv` flag typo ships to users:

- from the `root:root` state Docker actually leaves behind: healthy while
  unconfigured, host directories owned by `PUID`, `/proc/1/status` reporting
  uid 1000, `/data/seedmedic.db` present, and `docker stop` returning in under
  ten seconds.
- a hard-linked file under the staging root keeps its owner while the staging
  root itself changes hands. This is the safety test; it must fail if the chown
  ever becomes recursive.
- `--user 65534:65534` starts rather than failing with `setpriv`'s exit 127,
  which reads as "command not found".
- both spellings of the one-shot `--check-config` invocation.
- `docker compose config` resolves with and without `.env`.

## Acceptance criteria

- `docker compose up -d` in a fresh checkout, with no other command, produces a
  healthy container.
- No file exists that the operator did not cause, beyond the database.
- The staging root SeedMedic reports at `/status` is a string the download
  client can resolve.
- `/srv/media` is not writable from inside the container.
- The image runs as `PUID`, not as root, and stops gracefully.
- Both registries carry the same digest, for both architectures.

## Out of scope

- **A path-mapping facility for the download client.** Real, and the honest fix
  for the constraint this document works around — but a wrong mapping means
  writing to the wrong place, so it is a config-model change with its own safety
  questions, not a packaging one.
- `bootstrap::build_http_client`'s missing timeout. Recorded in 0019 as "Real,
  but separate", and still is.
- Publishing binaries, `cargo install`, or a crates.io release.
- Kubernetes manifests, Helm, Unraid or CasaOS templates.
- Rootless Docker and Podman specifics.
- Any change to how `staging.root` is validated in Rust.

## Open questions

- Should the entrypoint seed `/data/config.toml` with the container's own mount
  layout?

  **Resolved:** no — see the invariant above. The distinction between the
  application inventing a path and the image declaring its own layout is real,
  and it is still the wrong trade: it turns an honest "no candidate source is
  configured" warning into either a hard startup failure or a silent claim to
  have a library, and it removes the only forcing function for the qBittorrent
  mount the image cannot create. The operator learns the two paths from a
  first-run log line, the compose comments and the README instead.

- Should the health check be a `--health` flag in `main.rs` rather than bash?

  **Resolved:** no. It is the only option that can never go stale, and
  `reqwest` is already linked, so it looked like fifteen lines. It is not: it
  must load the configuration *without* validating it, since a broken file must
  not make an already-running server report unhealthy, and it must rewrite the
  unspecified bind address — `0.0.0.0` to `127.0.0.1`, `[::]` to `[::1]` —
  because those parse as a `SocketAddr` you cannot connect to. That is real,
  testable code added to a `main.rs` with no argument parser, to serve a case
  that has no good reason to occur inside a container. `bind_address` never
  applies without a restart, so it cannot drift during a run, and
  `SEEDMEDIC_HEALTH_PORT` fixes the remaining case from `.env` with no rebuild.

- Should `PUID`/`PGID` default to the previous 10001 instead of 1000?

  **Resolved:** no. 1000 is the first login account on almost every Linux host,
  so a bind-mount source the operator created already matches and nothing is
  chowned at all; it is what qBittorrent and the *arrs run as, which is what
  makes staged files usable; and it keeps `config.toml` editable without `sudo`,
  which `AGENTS.md` requires of it. Nothing depended on 10001, because no image
  was ever published. (For the record, the old image was `10001:999`, not
  `10001:10001` — `useradd --system --uid 10001` trips `SYS_UID_MAX` and
  allocates the gid from the system range, so the `chown -R 10001:10001` the old
  compose file recommended set a group the container was not in.)

- Should `linux/arm64` be cross-compiled from amd64 rather than built on a
  native runner?

  **Resolved:** not for now. QEMU is out — `libsqlite3-sys`, `cc` and `ring` are
  all in `Cargo.lock`, so an emulated release build runs for hours. That leaves
  a native runner, chosen here, or a cross toolchain in the builder stage
  (`gcc-aarch64-linux-gnu` plus the linker and `CC_` variables), which is free
  and fast but a second toolchain to maintain. Native runners are billed minutes
  on a private repository; if that becomes annoying, cross-compiling is the
  change to make, and it is confined to the builder stage.

- Should `.env.example`'s `DATA_PATH=./data` be renamed, since a developer
  running `cargo run` in the same checkout also writes `./data/seedmedic.db`?

  **Open.** They would share a database, which is arguably convenient and
  arguably a trap. Left alone until somebody is actually bitten.
