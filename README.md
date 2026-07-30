# SeedMedic

Detects hit-and-run warnings on private trackers and repairs them from media you
already have.

A hit-and-run happens when you download a torrent and fail to seed it for long
enough — usually because a client was reset, a disk was swapped, or an *arr moved
the files. The content is normally still in your library under a different name
and directory structure. SeedMedic finds it, rebuilds the layout the torrent
expects in a staging area, hands the torrent to qBittorrent paused, forces a hash
check, and starts seeding only when the data has been verified and your policy
allows it.

> **Status: feature-complete.** Every item on the roadmap in
> [`docs/todos/`](docs/todos/) is implemented and tested: the Unit3D tracker,
> qBittorrent, Sonarr/Radarr, bencode decoding, and reflink/hardlink/copy
> staging are all real, not stubs. The built-in fake tracker and download
> client are still there — gated behind the `fakes` feature, on by default —
> so the whole workflow stays runnable and demoable without touching a real
> service.
>
> What has *not* been exercised is a real private tracker — pointing a test
> suite at one is out of scope on purpose (see `tests/AGENTS.md`), so that
> side is only as proven as the wiremock tests make it. qBittorrent has been
> verified end to end against a real instance; see `docker-compose.test.yml`.
> The safety rules below do not change with any of this: nothing here trusts
> an adapter enough to skip them.

## Safety

SeedMedic operates on media it does not own and cannot replace. The priorities,
in order, are: never damage the library, never claim something happened that did
not, then repair the hit-and-run.

- The media library is opened read-only. Only the staging area is written to, and
  the staging root is proven at startup not to overlap any library root.
- Reflinks are preferred over hardlinks. A hardlinked staged file *is* the
  library file, so an incomplete hardlinked torrent is never resumed —
  qBittorrent would write the missing pieces into your media. No configuration
  option can turn that rule off.
- Exact file size is treated as evidence, not proof.
- Torrent paths are validated before anything touches the filesystem: no
  traversal, no absolute paths, no symlinked components.
- The default is to leave a repair paused for you to approve.
- A tracker saying the hit-and-run is cleared is the only thing that completes a
  repair. A happily seeding torrent proves nothing.

## Try it

```bash
cargo run
```

No configuration file needed for a first look: SeedMedic starts unconfigured,
logs a warning naming each setting that still needs one, and serves a page —
open <http://localhost:9899>. `/` and `/status` say plainly that nothing is
configured yet, and every page repeats it in a banner linking to
[`/settings`](http://localhost:9899/settings), which is where you fix it —
every setting is viewable and editable from the browser, with a form field
next to each, so a fresh install never needs a text editor at all.

Or with Docker — see [`docker-compose.yml`](docker-compose.yml):

```bash
docker compose up -d
```

That is the whole first run: nothing to create, nothing to `chown`, no
configuration file to copy. Docker makes the host directories, the container
takes ownership of the two it writes to, and it drops to `PUID`:`PGID` before
starting. Copy [`.env.example`](.env.example) to `.env` to change the uid, the
port, or where your media lives — every variable has a default, so that file is
optional. The image is on both registries:

```bash
docker pull ghcr.io/gnarr/seedmedic:latest
docker pull gnarr/seedmedic:latest
```

Two values go into `/settings`, and the container prints both in
`docker compose logs` on a fresh install: `/srv/media` for the library root,
and whatever you set `STAGING_PATH` to for the staging root. **If qBittorrent
runs in its own container, mount that same staging path into it at the same
string** — SeedMedic hands the path over verbatim as the torrent's save path
and nothing translates between the two, so a mismatch means the recheck finds
0% after all the staging work is already done.

To see a repair actually run without touching anything real, open
`/settings` and press **Load demo configuration** — it wires up the built-in
fake tracker and download client, and points `staging.root` at a directory
next to your config file. (Only offered on a fresh install with no trackers
configured yet, and only in a build with the `fakes` feature, which is on by
default.) The fake tracker reports two hit-and-runs on startup; with an empty
library they park for review with "no library file matches", which is the
correct outcome — `config.example.toml` has a three-line recipe for giving
them something to find and watching a repair run all the way through.

Prefer editing a file directly? `/settings` writes the same `config.toml` —
comments and key order preserved — so the two ways of configuring SeedMedic
never fight each other; copy [`config.example.toml`](config.example.toml)
(which documents every setting) to `config.toml` and edit it by hand exactly
as before.

## How it works

```
tracker ──1── .torrent ──2── library ──3── staging ──4── qBittorrent ──5── tracker
   discovery      inspection    matching    materialising    verify/seed   confirmation
```

Each repair is a row in SQLite with a state, advancing one step at a time:

```
discovered → torrent_fetched → matched → staged → injected
           → rechecking → verified → seeding → completed
                    ↘ awaiting_review ↘ failed
```

Every transition is a compare-and-swap written in the same database transaction
as its audit record, so the process can be killed at any point and pick up
cleanly. On startup, jobs are reconciled against the actual state of qBittorrent
and the staging directory — only ever backwards, because external state cannot
prove SeedMedic is the one that put it there.

Anything ambiguous, incomplete, or unsafe parks for review with a reason, and the
job page shows exactly which files were matched, why, and how they were staged.

## Configuration

One TOML file is the source of truth, and it stays that way — `/settings` in
the web UI is a second way to change it, not a replacement. Every setting is
viewable and editable there: a bad value gets a message next to the field
instead of a broken save, a secret is never displayed once set, and saving
writes `config.toml` back with comments and key order intact, touching only
the keys that changed, then reloads without a restart.

See [`config.example.toml`](config.example.toml), which
documents every setting. Point `SEEDMEDIC_CONFIG` at it, or leave it as
`./config.toml`; the Docker image sets it to `/data/config.toml`, alongside the
database. The file is optional: if it is absent, SeedMedic starts with
defaults and logs a warning naming the path it looked for and every setting
still unset, rather than refusing to start. A file that exists but does not
parse is still a hard error — a typo in a safety setting is never silently
replaced by a default.

The settings worth understanding before running it against anything real:

| Setting | Default | Why |
|---|---|---|
| `policy.auto_resume` | `never` | Nothing starts seeding without you. Set to `when_verified_complete` once you trust it. |
| `policy.allow_hardlink` | `false` | A hardlinked staged file is the library file. Leave this off unless you know why you want it. |
| `policy.min_match_confidence` | `probable` | `exact` only comes from a candidate whose bytes were hashed against the torrent's pieces and matched — see `policy.verification_pieces`. |
| `staging.root` | unset | Absolute, and not inside a library root, once set. Your download client must see it at the same path — SeedMedic hands it over verbatim. Unset, no repair can be materialized — it parks for review instead. |
| `download_client` | unset | Once set, `qbittorrent` needs both a username and password. Unset, no repair can be seeded — it parks for review instead. |
| `[[trackers]]` | none | Unset, discovery finds nothing — the correct state of a fresh install. |

Startup also checks the things that would otherwise fail confusingly hours
later: every tracker that needs an API key has one, library roots exist and
are readable, and the staging root's parent is writable.

```bash
seedmedic --check-config
```

Validates the configuration and prints a redacted summary of what was
understood, without opening the database, touching the network, or writing
anything — safe to run against a production config from anywhere.

```bash
seedmedic --check-connections
```

Proves each configured tracker, the download client, and every *arr instance
is reachable and its credentials are accepted — the same probes the settings
page's "Test connection" buttons run — and exits non-zero naming whichever
one failed. Unlike `--check-config`, this touches the network.

**The web UI has no accounts or roles.** By default it is unauthenticated — do
not expose it to the internet. Setting `server.auth_token` requires a
credential for every request but `/health` and `/login`: a script sends
`Authorization: Bearer <token>` exactly as before, and a browser signs in once
at `/login` and carries a session cookie after that. It is still a shared
secret, not a login system — no accounts, no roles, no password — and it does
not replace TLS or network-level access control. The settings page can also
make outbound requests to whatever address you type into it — its "Test
connection" buttons are an authenticated arbitrary-outbound-GET primitive by
design — which is another reason not to expose the port.

## Development

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
```

That runs the full suite except the tests marked `#[ignore]`: a 2000-file
scale test (slow, no other setup needed) and a live end-to-end run against a
real qBittorrent (see [`docker-compose.test.yml`](docker-compose.test.yml) for
a disposable instance to point it at):

```bash
cargo test --test scale -- --ignored --nocapture
cargo test --test live_qbittorrent -- --ignored --nocapture
```

- [`AGENTS.md`](AGENTS.md) — architecture, safety rules, and conventions. Read it
  before changing anything.
- [`docs/architecture.md`](docs/architecture.md) — the design and the reasoning.
- [`docs/todos/`](docs/todos/) — the implementation roadmap, in dependency order.

There are localised `AGENTS.md` files in
[`src/repair/`](src/repair/AGENTS.md), [`src/tracker/`](src/tracker/AGENTS.md),
[`src/staging/`](src/staging/AGENTS.md), [`migrations/`](migrations/AGENTS.md),
and [`tests/`](tests/AGENTS.md).

## Licence

MIT
