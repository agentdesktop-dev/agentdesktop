ARG BASE_IMAGE
FROM ${BASE_IMAGE}
RUN groupadd --system agentdesktop \
    && useradd --create-home --user-group --groups agentdesktop tester
USER tester
# The installer has no version argument: it installs the current release of
# the stable channel, so the test reads the version the launcher recorded.
RUN curl -fsSL https://api.meta.ai/install.sh -o /tmp/install-muse.sh \
    && MUSE_NO_MODIFY_PATH=1 bash /tmp/install-muse.sh \
    && rm /tmp/install-muse.sh \
    && MUSE_NO_AUTO_UPDATE=1 /home/tester/.local/bin/muse --version
USER root
ENV HOME=/home/tester \
    PATH=/home/tester/.local/bin:$PATH \
    MUSE_NO_AUTO_UPDATE=1
WORKDIR /home/tester/project
CMD ["sleep", "infinity"]
