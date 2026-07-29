# syntax=docker/dockerfile:1

FROM rust:1.96-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY migrations ./migrations
COPY src ./src
RUN --mount=type=cache,target=/app/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    cargo build --release && \
    mkdir -p /app/build && \
    cp /app/target/release/seedmedic /app/build/seedmedic

FROM debian:bookworm-slim

RUN useradd --system --create-home --uid 10001 seedmedic
WORKDIR /app
COPY --from=builder /app/build/seedmedic /usr/local/bin/seedmedic
COPY config.example.toml /app/config.example.toml
RUN mkdir -p /app/data /config && chown -R seedmedic:seedmedic /app/data /config

USER seedmedic
EXPOSE 9899
# May legitimately point at a file that does not exist yet: an empty /config
# volume is not an error. SeedMedic starts with defaults, logs a warning
# naming this path and every setting still unset, and serves a page saying
# the same — copy config.example.toml into the volume, or set the
# individual settings via environment variables (see below), when ready.
ENV SEEDMEDIC_CONFIG=/config/config.toml

# Secrets can be set here instead of in config.toml — see config.example.toml
# for the full precedence — or mounted as files and referenced with
# api_key_file / password_file / auth_token_file:
#   SEEDMEDIC_TRACKER_<ID>_API_KEY
#   SEEDMEDIC_ARR_<NAME>_API_KEY
#   SEEDMEDIC_DOWNLOAD_CLIENT_PASSWORD
#   SEEDMEDIC_SERVER_AUTH_TOKEN

# Mount the media library read-only. SeedMedic never writes to it, and the
# container should not be able to either.
#
# /config is pre-chowned above so an anonymous volume here (no explicit
# bind mount) is still writable by uid 10001 — otherwise the settings pages
# (docs/todos/0017-the-settings-pages.md) could view config.toml but never
# save it.
VOLUME ["/config", "/app/data", "/staging"]

CMD ["seedmedic"]
