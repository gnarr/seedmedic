# syntax=docker/dockerfile:1

# The one place the toolchain is named. CI passes this same value as a build
# arg and Cargo.toml's rust-version matches it, so the image and the tests are
# built by the same compiler by construction.
ARG RUST_VERSION=1.96

FROM rust:${RUST_VERSION}-bookworm AS builder
ARG TARGETARCH

WORKDIR /app
# Cargo.toml Cargo.lock — not Cargo.lock*. `--locked` below means nothing if a
# missing lockfile is quietly allowed to resolve a fresh one.
COPY Cargo.toml Cargo.lock ./
COPY migrations ./migrations
COPY src ./src
# The cache mounts cost nothing when cold (CI) and save minutes when warm (a
# developer's machine). Keyed by TARGETARCH so a local
# `buildx build --platform linux/amd64,linux/arm64` does not make the two
# architectures fight over one target directory.
RUN --mount=type=cache,id=seedmedic-target-${TARGETARCH},target=/app/target,sharing=locked \
    --mount=type=cache,id=cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry \
    --mount=type=cache,id=cargo-git-${TARGETARCH},target=/usr/local/cargo/git/db \
    cargo build --release --locked && \
    mkdir -p /app/build && \
    cp /app/target/release/seedmedic /app/build/seedmedic

FROM debian:bookworm-slim

# No package installs, deliberately:
#   - reqwest is rustls + webpki-roots, so there is no ca-certificates to add;
#   - setpriv is already here (util-linux is `Essential: yes`), so dropping
#     privileges needs no gosu or su-exec;
#   - the health check uses bash's /dev/tcp rather than curl, which would drag
#     libssl3 and fifteen other packages into an image that contains no
#     OpenSSL at all.

# Decorative: it makes `ls -l /data` and `ps` show a name in the default case.
# The entrypoint drops privileges by *number*, so PUID/PGID never need this
# entry to exist and never modify it — which is why there is no runtime
# usermod/groupmod and no uid/gid collision handling anywhere.
RUN groupadd --gid 1000 seedmedic \
 && useradd --uid 1000 --gid 1000 --no-create-home --shell /usr/sbin/nologin seedmedic

COPY --from=builder /app/build/seedmedic /usr/local/bin/seedmedic
COPY --chmod=0755 docker/entrypoint.sh /usr/local/bin/seedmedic-entrypoint
COPY config.example.toml /usr/share/seedmedic/config.example.toml

# Config and database in one directory: one thing to mount, one thing to back
# up. config.example.toml's `database.path` is the relative "data/seedmedic.db",
# which resolves against the process working directory — so `WORKDIR /` lands
# the database at /data/seedmedic.db, beside the config, with no change to the
# shipped example and no change to any Rust.
#
# That pairing is load-bearing. Do not "tidy" WORKDIR back to /app.
WORKDIR /
ENV SEEDMEDIC_CONFIG=/data/config.toml

# Nothing reads HOME today. This keeps a future dependency that does from
# trying to write to /root, which the dropped-privilege process cannot.
ENV HOME=/tmp

# Secrets can be set here instead of in config.toml — see config.example.toml
# for the full precedence — or mounted as files and referenced with
# api_key_file / password_file / auth_token_file:
#   SEEDMEDIC_TRACKER_<ID>_API_KEY
#   SEEDMEDIC_ARR_<NAME>_API_KEY
#   SEEDMEDIC_DOWNLOAD_CLIENT_PASSWORD
#   SEEDMEDIC_SERVER_AUTH_TOKEN

RUN mkdir -p /data /srv/media \
 && chown 1000:1000 /data

# No VOLUME. The old declaration named "/staging", a path that was never
# created, never chowned and referenced by nothing — and every entry in it
# had the same defect in waiting: VOLUME makes a bare `docker run` mint a
# root-owned anonymous volume, which is exactly the case
# docs/todos/0017-the-settings-pages.md had to detect and refuse in the UI.
# The entrypoint now fixes ownership at run time, so declaring volumes buys
# nothing and costs anonymous volumes that accumulate.
#
# The staging directory is deliberately not created here either: it is mounted
# at the same path inside and outside the container, so the image cannot know
# its name. An unwritable root filesystem is a feature — it means a mistyped
# staging root is refused inline by the settings page's writability check
# rather than silently working somewhere useless.

EXPOSE 9899

# /health is a readiness probe — 200 means the database is reachable and the
# worker has ticked recently — and is exempt from the auth token, so it needs
# no credential here. An unconfigured container is healthy on purpose: being
# unconfigured changes neither of those two facts.
#
# `bash -c` *is* the shell, so ${SEEDMEDIC_HEALTH_PORT:-9899} expands at
# container run time. Set it if you change server.bind_address's port, which
# never takes effect without a restart anyway.
HEALTHCHECK --interval=30s --timeout=5s --start-period=40s --retries=3 \
  CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/${SEEDMEDIC_HEALTH_PORT:-9899} && printf 'GET /health HTTP/1.1\\r\\nHost: localhost\\r\\nConnection: close\\r\\n\\r\\n' >&3 && head -1 <&3 | grep -q ' 200 '"]

# root on purpose, and only for as long as it takes. The entrypoint fixes
# ownership of the state directories, then execs
# `setpriv --reuid PUID --regid PGID`, so the server itself never runs
# privileged. `docker run --user ...` still works and skips both steps.
USER root
ENTRYPOINT ["/usr/local/bin/seedmedic-entrypoint"]
CMD ["seedmedic"]
