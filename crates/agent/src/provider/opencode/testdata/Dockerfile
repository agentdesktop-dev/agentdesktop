ARG BASE_IMAGE
FROM ${BASE_IMAGE}
ARG OPENCODE_VERSION=1.18.29
RUN groupadd --system agentdesktop \
    && useradd --create-home --user-group --groups agentdesktop tester
USER tester
RUN curl -fsSL https://opencode.ai/install -o /tmp/install-opencode.sh \
    && bash /tmp/install-opencode.sh --version "${OPENCODE_VERSION}" --no-modify-path \
    && rm /tmp/install-opencode.sh \
    && /home/tester/.opencode/bin/opencode --version
USER root
ENV HOME=/home/tester \
    PATH=/home/tester/.opencode/bin:$PATH \
    OPENCODE_DISABLE_AUTOUPDATE=1 \
    OPENCODE_DISABLE_MODELS_FETCH=1
WORKDIR /home/tester
CMD ["sleep", "infinity"]
