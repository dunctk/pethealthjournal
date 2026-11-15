FROM rust:1.97-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY templates ./templates
COPY static ./static
ARG GIT_SHA=""
ENV GIT_SHA=${GIT_SHA}
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
# Litestream: optional continuous SQLite backup (LITESTREAM_ENABLED=true).
ARG LITESTREAM_VERSION=0.5.17
ARG LITESTREAM_SHA256=cfb371176d164437ae869f8351cfde49bd1804ae71c61923f75c9cba9c9c006d
RUN curl -fsSL -o /tmp/litestream.tgz "https://github.com/benbjohnson/litestream/releases/download/v${LITESTREAM_VERSION}/litestream-${LITESTREAM_VERSION}-linux-x86_64.tar.gz" \
 && echo "${LITESTREAM_SHA256}  /tmp/litestream.tgz" | sha256sum -c - \
 && tar -xzf /tmp/litestream.tgz -C /usr/local/bin litestream \
 && rm /tmp/litestream.tgz
COPY deploy/litestream.yml /etc/litestream.yml
COPY deploy/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN useradd --system --uid 10001 --create-home pethealth && mkdir -p /persistent && chown pethealth:pethealth /persistent
COPY --from=builder /build/target/release/pethealth /usr/local/bin/pethealth
USER pethealth
ENV APP_HOST=0.0.0.0 APP_PORT=3000 PRODUCTION=true RUST_LOG=pethealth=info,tower_http=info
EXPOSE 3000
VOLUME ["/persistent"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 CMD curl -fsS http://127.0.0.1:3000/healthz || exit 1
CMD ["/usr/local/bin/entrypoint.sh"]

