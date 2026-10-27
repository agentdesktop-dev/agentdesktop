ARG BASE_IMAGE
FROM ${BASE_IMAGE}
# The real CLI is an npm package that needs Node. Discovery reads the
# `@github/copilot` manifest next to the launcher and never runs the tool, so
# an npm-shaped install of a stand-in launcher exercises the same code path:
# `<prefix>/bin/copilot` -> `<prefix>/lib/node_modules/@github/copilot/bin/copilot`.
ARG COPILOT_VERSION=1.0.88
RUN groupadd --system agentdesktop \
    && useradd --create-home --user-group --groups agentdesktop tester \
    && mkdir -p /usr/local/lib/node_modules/@github/copilot/bin \
    && printf '{"name":"@github/copilot","version":"%s"}\n' "${COPILOT_VERSION}" > /usr/local/lib/node_modules/@github/copilot/package.json \
    && printf '#!/bin/sh\necho "GitHub Copilot CLI %s."\n' "${COPILOT_VERSION}" > /usr/local/lib/node_modules/@github/copilot/bin/copilot \
    && chmod 0755 /usr/local/lib/node_modules/@github/copilot/bin/copilot \
    && ln -s ../lib/node_modules/@github/copilot/bin/copilot /usr/local/bin/copilot \
    && mkdir -p /run/user/1000 && chown tester:tester /run/user/1000
ENV HOME=/home/tester \
    XDG_RUNTIME_DIR=/run/user/1000
WORKDIR /home/tester
CMD ["sleep", "infinity"]
