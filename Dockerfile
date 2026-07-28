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
RUN mkdir -p /app/data && chown -R seedmedic:seedmedic /app/data

USER seedmedic
EXPOSE 9899
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
VOLUME ["/config", "/app/data", "/staging"]

CMD ["seedmedic"]
