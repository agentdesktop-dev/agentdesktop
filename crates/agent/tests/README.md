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
