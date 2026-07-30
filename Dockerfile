# syntax=docker/dockerfile:1

FROM docker.io/library/rust:1.97-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo build --locked --release

FROM cgr.dev/chainguard/wolfi-base:latest

RUN apk add --no-cache ca-certificates curl

COPY --from=builder /build/target/release/agentgateway-edge-connector /usr/local/bin/agentgateway-edge-connector

ENTRYPOINT ["agentgateway-edge-connector"]
