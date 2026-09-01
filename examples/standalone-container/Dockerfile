FROM docker.io/library/node:24-trixie AS web

WORKDIR /app/frontend
ENV CI=true
COPY frontend/package.json frontend/pnpm-lock.yaml frontend/pnpm-workspace.yaml ./
COPY frontend/controller/package.json controller/package.json
COPY frontend/desktop/package.json desktop/package.json
COPY frontend/ui/package.json ui/package.json
RUN corepack enable && pnpm install --frozen-lockfile
COPY frontend ./
COPY images /app/images
RUN pnpm --filter @agentdesktop/desktop-web build

FROM docker.io/library/rust:1.97.1-trixie AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        build-essential \
        libayatana-appindicator3-dev \
        librsvg2-dev \
        libssl-dev \
        libwebkit2gtk-4.1-dev \
        libxdo-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY images/mark.svg ./images/mark.svg
COPY --from=web /app/frontend/desktop/dist ./frontend/desktop/dist
COPY --from=web /app/frontend/desktop/assets ./frontend/desktop/assets
RUN cargo build --locked --release --package agentdesktop

FROM docker.io/library/node:24-trixie

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        libayatana-appindicator3-1 \
        libgtk-3-0 \
        libwebkit2gtk-4.1-0 \
    && npm install --global @anthropic-ai/claude-code \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home agentdesktop
COPY --from=builder /app/target/release/agentdesktop /usr/local/bin/agentdesktop
COPY examples/standalone-container/entrypoint.sh /usr/local/bin/demo-daemon
COPY examples/managed-container/entrypoint.sh /usr/local/bin/managed-demo-daemon
RUN chmod 0755 /usr/local/bin/demo-daemon /usr/local/bin/managed-demo-daemon

USER agentdesktop
WORKDIR /home/agentdesktop
ENTRYPOINT ["/usr/local/bin/demo-daemon"]