# syntax=docker/dockerfile:1

FROM cgr.dev/chainguard/rust:latest-dev AS builder

WORKDIR /build
COPY --chown=65532:65532 Cargo.toml Cargo.lock ./
COPY --chown=65532:65532 build.rs ./
COPY --chown=65532:65532 src ./src
COPY --chown=65532:65532 tests ./tests
RUN cargo build --locked --release

FROM ghcr.io/agentgateway/agentgateway:v1.4.1 AS agentgateway

FROM cgr.dev/chainguard/wolfi-base:latest

RUN apk add --no-cache ca-certificates curl

COPY --from=builder /build/target/release/agentdesktop /usr/local/bin/agentdesktop
COPY --from=agentgateway /app/agentgateway /usr/local/bin/agentgateway

ENTRYPOINT ["agentdesktop"]
