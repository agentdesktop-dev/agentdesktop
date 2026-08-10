# Agentplane

Agentplane currently consists of a small Linux daemon and a CLI client. At startup, the daemon reads a YAML configuration and discovers installed coding agents. It exposes its state through an HTTP API over a Unix domain socket.

Build it:

```console
cargo build
```

Run it as your user while developing:

```console
cargo run --bin agentplaned -- --config ./config.yaml --socket /tmp/agentplane.sock
```

In another terminal:

```console
cargo run --bin agentplane -- --socket /tmp/agentplane.sock status
cargo run --bin agentplane -- --socket /tmp/agentplane.sock discover
cargo run --bin agentplane -- --socket /tmp/agentplane.sock config
```

The production-oriented defaults are `/etc/agentplane/config.yaml` and `/run/agentplane/agentplane.sock`.
