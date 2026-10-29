# syntax=docker/dockerfile:1.26

# Device images for the enrollment lab: `headless` runs the headless daemon,
# `desktop` runs the daemon and the desktop app in an XFCE session served
# over noVNC.

FROM docker.io/library/node:26.11.1-bookworm AS ui
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
    pnpm --filter @agentdesktop/desktop-web build

# The headless daemon needs neither the UI nor WebKit, so the `headless`
# target, which the e2e test uses, builds it on its own.
FROM docker.io/library/rust:1.99.0-trixie AS headless-builder
ARG TARGETARCH
ARG BUILD_PROFILE=release
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY images ./images
RUN --mount=type=cache,id=agentdesktop-lab-headless-target-${TARGETARCH},target=/app/target \
    --mount=type=cache,id=agentdesktop-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=agentdesktop-cargo-git,target=/usr/local/cargo/git \
    case "${BUILD_PROFILE}" in \
      release) flags=--release ;; \
      debug) flags= ;; \
      *) echo "unsupported BUILD_PROFILE: ${BUILD_PROFILE}" >&2; exit 1 ;; \
    esac && \
    cargo build --locked ${flags} --package agentdesktop-agent --bin agentdesktop-headless && \
    cp "target/${BUILD_PROFILE}/agentdesktop-headless" /usr/local/bin/

FROM docker.io/library/rust:1.99.0-trixie AS builder
ARG TARGETARCH
ARG BUILD_PROFILE=release
RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        libwebkit2gtk-4.1-dev \
        libxdo-dev \
        libssl-dev \
        libayatana-appindicator3-dev \
        librsvg2-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY images ./images
COPY frontend/desktop/assets frontend/desktop/assets
COPY --from=ui /app/frontend/desktop/dist frontend/desktop/dist
RUN --mount=type=cache,id=agentdesktop-lab-target-${TARGETARCH},target=/app/target \
    --mount=type=cache,id=agentdesktop-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=agentdesktop-cargo-git,target=/usr/local/cargo/git \
    case "${BUILD_PROFILE}" in \
      release) flags=--release ;; \
      debug) flags= ;; \
      *) echo "unsupported BUILD_PROFILE: ${BUILD_PROFILE}" >&2; exit 1 ;; \
    esac && \
    cargo build --locked ${flags} --package agentdesktop --bin agentdesktop && \
    cp "target/${BUILD_PROFILE}/agentdesktop" /usr/local/bin/

FROM docker.io/library/debian:trixie-slim AS headless
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system agentdesktop
COPY --from=headless-builder /usr/local/bin/agentdesktop-headless /usr/local/bin/agentdesktop-headless
ENTRYPOINT ["/usr/local/bin/agentdesktop-headless"]
CMD ["daemon", "--config", "/demo/agentdesktop.yaml"]

FROM docker.io/library/debian:trixie AS desktop
RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        ca-certificates \
        dbus-x11 \
        firefox-esr \
        fonts-dejavu \
        libayatana-appindicator3-1 \
        libwebkit2gtk-4.1-0 \
        novnc \
        supervisor \
        tigervnc-standalone-server \
        websockify \
        xdg-utils \
        xfce4-panel \
        xfce4-session \
        xfce4-settings \
        xfce4-terminal \
        xfdesktop4 \
        xfwm4 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system agentdesktop \
    && useradd --create-home --shell /bin/bash --groups agentdesktop dev
COPY --from=builder /usr/local/bin/agentdesktop /usr/local/bin/agentdesktop
COPY examples/enrollment/desktop/supervisord.conf /etc/supervisor/conf.d/lab.conf
COPY examples/enrollment/desktop/session.sh /usr/local/bin/lab-session
COPY examples/enrollment/desktop/agentdesktop.desktop /etc/xdg/autostart/agentdesktop.desktop
COPY examples/enrollment/desktop/firefox-policies.json /usr/lib/firefox-esr/distribution/policies.json
RUN chmod 0755 /usr/local/bin/lab-session
ENTRYPOINT ["/usr/bin/supervisord", "--nodaemon", "--configuration", "/etc/supervisor/supervisord.conf"]
