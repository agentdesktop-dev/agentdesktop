ARG BASE_IMAGE
FROM ${BASE_IMAGE}
ARG CODEX_VERSION=0.153.4
RUN groupadd --system agentdesktop \
    && useradd --create-home --user-group --groups agentdesktop tester
USER tester
RUN curl -fsSL "https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/install.sh" -o /tmp/install-codex.sh \
    && sh /tmp/install-codex.sh --release "${CODEX_VERSION}" \
    && rm /tmp/install-codex.sh \
    && /home/tester/.local/bin/codex --version
USER root
ENV HOME=/home/tester \
    PATH=/home/tester/.local/bin:$PATH
WORKDIR /home/tester
CMD ["sleep", "infinity"]
