# Provider integration tests

Run the Docker suite explicitly:

```sh
cargo test -p agentdesktop-agent --test integration -- --nocapture
```

Filter to one provider with a test-name substring, for example:

```sh
cargo test -p agentdesktop-agent --test integration claude_code -- --nocapture
```

A local Linux Docker engine must be accessible to the current user.
The suite uses testcontainers through the Docker API; no Docker CLI is required.
Missing Docker is an error when running this suite. To run only library unit
tests, use:

```sh
cargo test -p agentdesktop-agent --lib
```

Plain `cargo test` runs both library and integration tests.

Tracing uses compact text output and defaults to `info`. Cargo captures logs
normally; `--nocapture` displays them live. Use `RUST_LOG` for more detail,
including container commands and timings:

```sh
RUST_LOG=integration=debug cargo test -p agentdesktop-agent --test integration -- --nocapture
```

Command output and background service logs remain in the failure artifacts.

Cargo builds the headless Agentdesktop executable and the test mounts it read-only
at `/usr/local/bin/agentdesktop`. The image contains the pinned Claude Code release; daemon changes do not rebuild the image. Docker caches
the provider installation between runs.

Run the suite on Linux with a local Docker engine. The binary must match the
container's architecture and libc; startup checks that it can execute.

Each scenario gets a fresh container with only the executable mounted and
`--network host`.
The installed provider talks to an Axum gateway fixture in the Rust test process
through loopback on a dynamically allocated port. No external account or paid inference is needed; the fixture uses a static test API key. No controller is started by this scenario;
a controller can also run on the host when testing against a real backend.
Containers are removed after success or failure.
Commands have deadlines, including image builds. On failure, the test reports an
artifact directory containing commands, stdout/stderr, and background service logs.

There is one Rust integration-test target, `integration.rs`. Provider scenarios
live in `src/provider/<provider>/integration_tests.rs` and are included using
`#[path]`. Dockerfiles and supporting fixtures live beside those scenarios.
Shared container operations live in `tests/common`. Scenarios use `#[tokio::test]`;
the gateway runs on the same runtime. Testcontainers builds images and manages
container startup, execution, and automatic cleanup, including on panic.

The initial Claude Code scenario covers discovery, dry run, system-managed gateway
settings, a request using a managed test API key, repeat application, and cleanup
preserving user and separately managed settings. OIDC login, credential-helper
authentication, tool hooks, and sandbox enforcement are not covered by this scenario.

To change the tested Claude Code version, update the Dockerfile's pinned version
and the scenario's expected version together.

Provider Dockerfiles use `ARG BASE_IMAGE` and `FROM ${BASE_IMAGE}`. The shared
harness supplies a digest-pinned `istio/base` image with curl, CA certificates,
and network debugging tools, so common prerequisites need no package-install step.
Only the Dockerfile is sent as build context. Builds use unique tags to keep
parallel runs independent while reusing Docker’s layer cache.
