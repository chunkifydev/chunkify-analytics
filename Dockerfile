# syntax=docker/dockerfile:1
FROM rust:1.99.0-bookworm@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6 AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY openapi.json ./
ARG TARGETARCH
ARG BUILD_JOBS=2
# CI changes this value to run Cargo even when image layers were restored.
# Unlike --no-cache, this preserves the dependency cache mounts.
ARG CI_BUILD_ID=local
RUN --mount=type=cache,id=collector-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=collector-target-${TARGETARCH},target=/app/target \
    printf 'Cargo build check: %s\n' "$CI_BUILD_ID" \
    && cargo clean --release --package chunkify-analytics \
    && cargo build --locked --release --jobs "$BUILD_JOBS" \
    && cp target/release/chunkify-analytics /usr/local/bin/chunkify-analytics

FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
# Snapshot repositories keep runtime package versions fixed alongside Cargo.lock.
RUN rm -f /etc/apt/sources.list.d/debian.sources \
    && printf '%s\n' \
       'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20261005T000000Z bookworm main' \
       'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian-security/20261005T000000Z bookworm-security main' \
       > /etc/apt/sources.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libstdc++6 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 collector \
    && useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin collector \
    && install -d -m 0700 -o 10001 -g 10001 /data
COPY --from=build /usr/local/bin/chunkify-analytics /usr/local/bin/
ENV COLLECTOR_DATA_DIR=/data COLLECTOR_BIND=0.0.0.0:4174
USER 10001:10001
WORKDIR /data
EXPOSE 4174
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --start-interval=1s --retries=3 \
    CMD curl --noproxy '*' --fail --silent --show-error --max-time 2 "http://${COLLECTOR_BIND}/healthz" > /dev/null || exit 1
ENTRYPOINT ["/usr/local/bin/chunkify-analytics"]
CMD ["serve"]
