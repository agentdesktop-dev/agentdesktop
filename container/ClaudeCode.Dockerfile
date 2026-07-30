FROM cgr.dev/chainguard/wolfi-base:latest

ARG CLAUDE_CODE_VERSION=2.1.212
RUN apk add --no-cache nodejs npm \
    && npm install --global \
        --allow-scripts=@anthropic-ai/claude-code \
        "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
    && npm cache clean --force \
    && apk del npm

USER nonroot
WORKDIR /home/nonroot
ENTRYPOINT ["claude"]