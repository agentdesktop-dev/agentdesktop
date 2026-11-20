# syntax=docker/dockerfile:1.26@sha256:ecfaec9ed6d810b56388c508f4121597bfbba70d41a6dfeee4d8cad5f295fc32

FROM docker.io/library/node:26.13.1-bookworm@sha256:b7087bc4d973034b30854b378ba107f9fab622b47514cb7e2592e41b8bab8d49 AS ui
WORKDIR /app/frontend
ENV CI=true
COPY frontend/package.json frontend/pnpm-lock.yaml frontend/pnpm-workspace.yaml ./
COPY frontend/controller/package.json controller/package.json
COPY frontend/desktop/package.json desktop/package.json
COPY frontend/ui/package.json ui/package.json
RUN npm install --global pnpm@11.25.0 && pnpm config set store-dir /pnpm/store
RUN --mount=type=cache,id=agentdesktop-pnpm,target=/pnpm/store \
    pnpm install --frozen-lockfile
COPY frontend/ ./
COPY images/ /app/images/
RUN --mount=type=cache,id=agentdesktop-pnpm,target=/pnpm/store \
    pnpm --filter @agentdesktop/controller-web build

FROM docker.io/library/rust:1.99.1-trixie@sha256:8a4e90c7946b74626488b1446f712c0fe0ed1a596cbd3d871684bc9c5433ae07 AS builder
ARG TARGETARCH
ARG BUILD_PROFILE=release
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY --from=ui /app/frontend/controller/dist frontend/controller/dist
RUN --mount=type=cache,id=agentdesktop-target-${TARGETARCH},target=/app/target \
    --mount=type=cache,id=agentdesktop-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=agentdesktop-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    case "${BUILD_PROFILE}" in \
      release) cargo build --locked --release --package agentdesktop-controller ;; \
      debug) cargo build --locked --package agentdesktop-controller ;; \
      *) echo "unsupported BUILD_PROFILE: ${BUILD_PROFILE}" >&2; exit 1 ;; \
    esac && \
    cp "target/${BUILD_PROFILE}/agentdesktop-controller" /agentdesktop-controller

FROM cgr.dev/chainguard/glibc-dynamic:latest@sha256:3359c6ba6b3bcf3b1d2179849f782365564cb262789e732e2833b25b9a8c00f1
COPY --from=builder /agentdesktop-controller /agentdesktop-controller
ENTRYPOINT ["/agentdesktop-controller"]
