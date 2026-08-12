# syntax=docker/dockerfile:1.11

FROM docker.io/library/node:24.17.0-bookworm AS ui
WORKDIR /app/ui
COPY ui/package.json ui/pnpm-lock.yaml ./
RUN corepack enable
RUN --mount=type=cache,id=agentdesktop-pnpm,target=/pnpm/store \
    pnpm install --frozen-lockfile --store-dir=/pnpm/store
COPY ui/ ./
RUN pnpm frontend:build

FROM docker.io/library/rust:1.97.0-alpine AS builder
RUN apk add --no-cache cmake make musl-dev perl protobuf
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY ui/src-tauri ui/src-tauri
COPY --from=ui /app/ui/dist ui/dist
RUN --mount=type=cache,id=agentdesktop-target,target=/app/target \
    --mount=type=cache,id=agentdesktop-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=agentdesktop-cargo-git,target=/usr/local/cargo/git \
    cargo build --locked --release --package agentdesktop-controller && \
    cp target/release/agentdesktop-controller /agentdesktop-controller

FROM cgr.dev/chainguard/static:latest
COPY --from=builder /agentdesktop-controller /agentdesktop-controller
ENTRYPOINT ["/agentdesktop-controller"]
