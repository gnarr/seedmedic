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

> **Status: bootstrap.** The workflow, the durable state machine, staging,
> matching, the operator UI, and configuration are implemented and tested. The
> external integrations — Unit3D trackers, qBittorrent, Sonarr/Radarr, bencode
> decoding, reflinks — are stubs that fail loudly and point at their
> implementation documents in [`docs/todos/`](docs/todos/). Built-in fake
> adapters make the whole thing runnable and testable in the meantime.
>
> It will not repair a real hit-and-run yet. It will not damage anything either.

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
cp config.example.toml config.toml
# edit staging.root and library.roots to real absolute paths
cargo run
```

Open <http://localhost:9899>. The built-in fake tracker reports two hit-and-runs
on startup. With an empty library they park for review with "no library file
matches", which is the correct outcome — `config.example.toml` has a three-line
recipe for giving them something to find and watching a repair run all the way
through.

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

One TOML file — see [`config.example.toml`](config.example.toml), which
documents every setting. Point `SEEDMEDIC_CONFIG` at it, or leave it as
`./config.toml`.

The settings worth understanding before running it against anything real:

| Setting | Default | Why |
|---|---|---|
| `policy.auto_resume` | `never` | Nothing starts seeding without you. Set to `when_verified_complete` once you trust it. |
| `policy.allow_hardlink` | `false` | A hardlinked staged file is the library file. Leave this off unless you know why you want it. |
| `policy.min_match_confidence` | `probable` | `exact` needs piece verification, which is [TODO 0005](docs/todos/0005-media-matching.md). |
| `staging.root` | — | Absolute, and not inside a library root. Startup refuses otherwise. |

Startup also checks the things that would otherwise fail confusingly hours
later: every tracker that needs an API key has one, library roots exist and
are readable, and the staging root's parent is writable.

```bash
seedmedic --check-config
```

Validates the configuration and prints a redacted summary of what was
understood, without opening the database, touching the network, or writing
anything — safe to run against a production config from anywhere.

**The web UI has no accounts or roles.** By default it is unauthenticated — do
not expose it to the internet. Setting `server.auth_token` requires every
request but `/health` to send `Authorization: Bearer <token>`, which is enough
to keep it off casual scans behind a reverse proxy; it is a shared secret, not
a login system, and does not replace TLS or network-level access control.

## Development

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
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
