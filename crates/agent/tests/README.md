# Provider integration tests

Requires Linux and a local Docker engine. Tests install pinned providers in
containers and mount the Cargo-built daemon. No account or API key is needed.

```sh
cargo test -p agentdesktop-agent --test integration -- --nocapture
# Run one provider:
cargo test -p agentdesktop-agent --test integration codex -- --nocapture
```

Use `RUST_LOG=integration=debug` for detailed logs. Failed tests print the path to
their saved artifacts.

# Intercept listener test

`intercept.rs` runs the Claude Desktop intercept listener end to end on
loopback through its public API, with a fake gateway and a fresh intercept CA.
It needs no Docker, keychain or admin rights, and runs on every platform. Each
step prints what it shows:

```sh
cargo test -p agentdesktop-agent --test intercept -- --nocapture
```
