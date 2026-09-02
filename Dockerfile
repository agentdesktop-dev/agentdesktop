# syntax=docker/dockerfile:1.11

FROM docker.io/library/node:26.8.1-bookworm AS ui
WORKDIR /app/frontend
ENV CI=true
COPY frontend/package.json frontend/pnpm-lock.yaml frontend/pnpm-workspace.yaml ./
COPY frontend/controller/package.json controller/package.json
COPY frontend/desktop/package.json desktop/package.json
COPY frontend/ui/package.json ui/package.json
RUN corepack enable && pnpm config set store-dir /pnpm/store
RUN --mount=type=cache,id=agentdesktop-pnpm,target=/pnpm/store \
    pnpm install --frozen-lockfile
COPY frontend/ ./
COPY images/ /app/images/
RUN --mount=type=cache,id=agentdesktop-pnpm,target=/pnpm/store \
    pnpm --filter @agentdesktop/controller-web build

FROM docker.io/library/rust:1.97.1-trixie AS builder
ARG TARGETARCH
ARG BUILD_PROFILE=release
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY --from=ui /app/frontend/controller/dist frontend/controller/dist
RUN --mount=type=cache,id=agentdesktop-target-${TARGETARCH},target=/app/target \
    --mount=type=cache,id=agentdesktop-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=agentdesktop-cargo-git,target=/usr/local/cargo/git \
    case "${BUILD_PROFILE}" in \
      release) cargo build --locked --release --package agentdesktop-controller ;; \
      debug) cargo build --locked --package agentdesktop-controller ;; \
      *) echo "unsupported BUILD_PROFILE: ${BUILD_PROFILE}" >&2; exit 1 ;; \
    esac && \
    cp "target/${BUILD_PROFILE}/agentdesktop-controller" /agentdesktop-controller

FROM cgr.dev/chainguard/glibc-dynamic:latest
COPY --from=builder /agentdesktop-controller /agentdesktop-controller
ENTRYPOINT ["/agentdesktop-controller"]
