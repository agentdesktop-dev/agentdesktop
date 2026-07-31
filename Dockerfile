# syntax=docker/dockerfile:1

FROM cgr.dev/chainguard/rust:latest-dev AS builder

WORKDIR /build
COPY --chown=65532:65532 Cargo.toml Cargo.lock ./
COPY --chown=65532:65532 src ./src
COPY --chown=65532:65532 tests ./tests
RUN cargo build --locked --release

FROM cgr.dev/chainguard/wolfi-base:latest

RUN apk add --no-cache ca-certificates curl

COPY --from=builder /build/target/release/agentgateway-edge-connector /usr/local/bin/agentgateway-edge-connector

ENTRYPOINT ["agentgateway-edge-connector"]
