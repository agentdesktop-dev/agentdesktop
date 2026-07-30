FROM cgr.dev/chainguard/wolfi-base:latest

RUN apk add --no-cache nodejs
COPY mock-anthropic.mjs /mock-anthropic.mjs

USER nonroot
ENTRYPOINT ["node", "/mock-anthropic.mjs"]