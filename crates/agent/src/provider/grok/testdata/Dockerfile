ARG BASE_IMAGE
FROM ${BASE_IMAGE}
ARG GROK_VERSION=1.0.13
RUN groupadd --system agentdesktop \
    && useradd --create-home --user-group --groups agentdesktop tester
USER tester
RUN curl -fsSL https://x.ai/cli/install.sh -o /tmp/install-grok.sh \
    && bash /tmp/install-grok.sh "${GROK_VERSION}" \
    && rm /tmp/install-grok.sh \
    && /home/tester/.grok/bin/grok --version
USER root
ENV HOME=/home/tester \
    PATH=/home/tester/.grok/bin:$PATH
WORKDIR /home/tester/project
CMD ["sleep", "infinity"]
