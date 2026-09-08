ARG BASE_IMAGE
FROM ${BASE_IMAGE}
ARG CURSOR_VERSION=3.19.13
ARG CURSOR_COMMIT=dd066f332fcea7382764400fde902f61920648d5
# The CLI links Electron libraries but does not need a display server.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libnss3 libatk-bridge2.0-0 libgtk-3-0 libgbm1 libasound2t64 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system agentdesktop \
    && useradd --create-home --user-group --groups agentdesktop tester
RUN case "$(uname -m)" in x86_64) platform=x64; arch=amd64 ;; aarch64) platform=arm64; arch=arm64 ;; *) exit 1 ;; esac \
    && curl -fsSL "https://downloads.cursor.com/production/${CURSOR_COMMIT}/linux/${platform}/deb/${arch}/deb/cursor_${CURSOR_VERSION}_${arch}.deb" -o /tmp/cursor.deb \
    && dpkg-deb -x /tmp/cursor.deb / \
    && rm /tmp/cursor.deb \
    && ln -s cursor /usr/share/cursor/bin/code
# Simulate Cursor being installed as the user's code command as well.
ENV HOME=/home/tester \
    PATH=/usr/share/cursor/bin:$PATH
WORKDIR /home/tester/project
CMD ["sleep", "infinity"]
