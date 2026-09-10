ARG BASE_IMAGE
FROM ${BASE_IMAGE}
# Use the native installer with an exact version, preserving the installation
# layout that production discovery reads (including the version directory).
ARG CLAUDE_CODE_VERSION=2.1.237
RUN groupadd --system agentdesktop \
    && useradd --create-home --user-group --groups agentdesktop tester
USER tester
RUN curl -fsSL https://claude.ai/install.sh -o /tmp/install-claude.sh \
    && bash /tmp/install-claude.sh "${CLAUDE_CODE_VERSION}" \
    && rm /tmp/install-claude.sh \
    && /home/tester/.local/bin/claude --version
USER root
ENV HOME=/home/tester \
    PATH=/home/tester/.local/bin:$PATH \
    DISABLE_AUTOUPDATER=1 \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
WORKDIR /home/tester
CMD ["sleep", "infinity"]
