# syntax=docker/dockerfile:1
# commonsd only: cargo builds the server crate and its dependencies, never the desktop crates.
FROM rust:1.99.1-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p server --bin commonsd \
    && cp target/release/commonsd /usr/local/bin/commonsd

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --user-group --home-dir /var/lib/commons commons \
    && mkdir -p /var/lib/commons/files \
    && chown -R commons:commons /var/lib/commons
COPY --from=build /usr/local/bin/commonsd /usr/local/bin/commonsd
USER commons
WORKDIR /var/lib/commons
ENV COMMONS_BIND=0.0.0.0:8080 \
    COMMONS_FILES_DIR=/var/lib/commons/files \
    RUST_LOG=info
EXPOSE 8080
# commonsd shuts down gracefully on SIGINT; as PID 1 it would ignore docker's default SIGTERM.
STOPSIGNAL SIGINT
ENTRYPOINT ["/usr/local/bin/commonsd"]
