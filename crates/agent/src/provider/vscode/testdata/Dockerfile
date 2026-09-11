ARG BASE_IMAGE
FROM ${BASE_IMAGE}
ARG VSCODE_VERSION=1.136.2
# Electron's CLI still links these libraries; no display server is needed.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libnss3 libatk-bridge2.0-0 libgtk-3-0 libgbm1 libasound2t64 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system agentdesktop \
    && useradd --create-home --user-group --groups agentdesktop tester
RUN case "$(uname -m)" in x86_64) arch=x64 ;; aarch64) arch=arm64 ;; *) exit 1 ;; esac \
    && curl -fsSL "https://update.code.visualstudio.com/${VSCODE_VERSION}/linux-${arch}/stable" -o /tmp/code.tar.gz \
    && mkdir -p /opt/vscode \
    && tar -xzf /tmp/code.tar.gz --strip-components=1 -C /opt/vscode \
    && rm /tmp/code.tar.gz \
    && ln -s /opt/vscode/bin/code /usr/local/bin/code
ENV HOME=/home/tester
WORKDIR /home/tester/project
CMD ["sleep", "infinity"]
