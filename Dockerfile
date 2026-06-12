FROM rust:1.95-bookworm AS builder

WORKDIR /build
COPY . .
RUN cargo build --locked --release --features server --bin loomabase-server

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 loomabase \
    && useradd --uid 10001 --gid loomabase --no-create-home --shell /usr/sbin/nologin loomabase

COPY --from=builder /build/target/release/loomabase-server /usr/local/bin/loomabase-server

USER 10001:10001
ENV LOOMABASE_BIND=0.0.0.0:8080
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:8080/health || exit 1

ENTRYPOINT ["/usr/local/bin/loomabase-server"]
