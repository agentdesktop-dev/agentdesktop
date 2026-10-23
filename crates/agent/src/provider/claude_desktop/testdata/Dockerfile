ARG BASE_IMAGE
FROM ${BASE_IMAGE}
# Claude Desktop has no Linux release, so this image runs only the daemon and
# curl, which stands in for Claude Desktop behind the intercept listener.
RUN groupadd --system agentdesktop
CMD ["sleep", "infinity"]
