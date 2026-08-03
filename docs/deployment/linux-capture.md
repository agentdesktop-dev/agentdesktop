# Linux capture prototype

The first Linux transparent-capture mechanism targets an externally managed cgroup v2 scope and nftables. A separate unprivileged relay recovers the original destination and opens one HTTP/2 CONNECT stream per redirected TCP flow. It is a prototype boundary and does not enable the `captured` application path yet.

## Behavior

The privileged setup binary owns one `inet agentdesktop` table. For every component in the configured absolute cgroup path, it emits a `socket cgroupv2 level` match. This selects the scope and descendant cgroups, so child and helper processes remain in the application profile without executable-name matching.

For matching sockets, the table:

- Redirects TCP destination port 443 to the configured local capture listener.
- Rejects UDP destination port 443 so QUIC cannot bypass inspection.
- Counts both actions without logging destinations, processes, or payloads.
- Leaves all other traffic and all other cgroups unchanged.

Rules are ephemeral and do not survive reboot. Installation replaces the connector-owned table in one nftables transaction. Removal deletes only that table and is a no-op when it is absent.

## Setup boundary

The application scope must already exist. Render rules without privilege:

```bash
cargo run --bin agentdesktop-capture-setup -- render \
  --cgroup /user.slice/user-1000.slice/app.slice/claude.scope \
  --redirect-port 15001
```

Preflight, installation, and removal require root because nftables validation uses kernel netlink:

```bash
sudo agentdesktop-capture-setup preflight \
  --cgroup /user.slice/user-1000.slice/app.slice/claude.scope \
  --redirect-port 15001
sudo agentdesktop-capture-setup install \
  --cgroup /user.slice/user-1000.slice/app.slice/claude.scope \
  --redirect-port 15001
sudo agentdesktop-capture-setup remove
```

Do not place the connector or Agent Gateway in the selected application scope. Doing so can redirect the tunnel transport back into the capture listener.

Start the prototype relay outside the selected scope after an HBONE listener is ready:

```bash
cargo run --bin agentdesktop -- capture \
  --listen 127.0.0.1:15001 \
  --hbone-endpoint 127.0.0.1:15008 \
  --token-file "$XDG_RUNTIME_DIR/agentdesktop/capture-token"
```

The capture relay is a connector subcommand rather than a separate product binary. The token file is required, must be a regular file owned by the current user with mode `0600`, and must contain a non-empty HTTP header value. Agent Gateway must receive the same value through its protected startup environment and authorize the re-entered route with `source.connectHeaders["x-agentdesktop-token"]`. The smoke config demonstrates this policy without creating a second policy format.

Both endpoints are restricted to loopback. The relay uses Linux `SO_ORIGINAL_DST`, preserves raw bytes, bounds concurrent tunnels, and closes failed or overloaded flows without direct fallback. Its cleartext HBONE pool reconnects lazily for later flows after observed transport loss; it never retries a failed CONNECT or replays inner bytes. The opaque token prevents unrelated local clients from using a correctly configured tunnel listener, but creation, rotation, and process lifecycle are not integrated. Managed use additionally requires TLS and DPoP-bound organizational identity.

## Isolated validation

Run the opt-in behavior test with Podman:

```bash
sh tests/linux-capture-container.sh
```

The test uses rootless Podman with private network and cgroup namespaces. Rootless `--privileged` grants the disposable container enough namespaced privilege to create a child cgroup and nftables table; it does not use host network or cgroup namespaces. It builds and runs the real relay against a minimal packaged Python h2 peer, then proves original-destination CONNECT authority, bidirectional bytes, TCP/443 redirection, UDP/443 rejection, counters, and scoped table removal. It downloads Rust dependencies, nftables, and small networking tools into the disposable container.

This does not prove production host attribution, resistance to a local administrator, or compatibility with an existing host firewall. Those require a disposable host or VM.

## Application execution scope

The public launch boundary can already place a command and all descendants in an owned transient systemd user scope:

```bash
agentdesktop launch --profile claude -- claude
```

The embedded `claude` profile supplies `ANTHROPIC_BASE_URL` and the local placeholder credential only to the launched process tree, so this path does not modify Claude settings. The default `custom` profile supplies no integration environment.

Before launching Claude, the profile checks `/_agentdesktop/healthz` with bounded local timeouts. A stopped connector or unreachable Agent Gateway produces immediate service-start, installation, and retry guidance. `--skip-preflight` bypasses readiness only for deliberate debugging; it does not remove the profile environment or execution scope.

This command currently provides process grouping, profile environment, and exit-status propagation only. It does not invoke the capture helper or claim network isolation. The capture-session controller will use the same boundary with a gated child: create the scope, keep the application stopped, resolve and validate the exact cgroup path, activate trust and nftables capture, and only then release the command. Scope emptiness and controller state, not Claude hooks or individual PIDs, determine cleanup.

The launch interface may later select a stronger sandbox or VM backend. Those backends must preserve the same full-process-tree, explicit-guarantee, fail-closed, and ordered-cleanup semantics; they must not silently replace unavailable isolation with the current process-only scope.

## Real Agent Gateway interoperability

Run the opt-in local smoke test against a built Agent Gateway checkout:

```bash
AGENTGATEWAY_BINARY=../agentgateway/target/debug/agentgateway \
  sh tests/agentgateway-hbone-smoke.sh
```

The test starts the config in `container/agentgateway-hbone-smoke.yaml`, the real connector relay, and a loopback HTTP target. It verifies that Agent Gateway accepts the connector's HTTP/2 CONNECT, re-enters an internal wildcard route, dynamically forwards the inner request, and returns the response. It does not install nftables; the private-container test covers the kernel redirect and original-destination boundary separately.

This smoke path generates an ephemeral token, proves that Agent Gateway returns `403` for a wrong CONNECT token, then proves dynamic forwarding succeeds with the protected valid token. It is intentionally loopback-only and does not prove the managed identity contract or a secure remote production tunnel.

## eBPF strengthening path

An eBPF cgroup `connect4`/`connect6` implementation can improve the production design by selecting sockets at connect time, preserving destination metadata without conntrack lookup, reducing interaction with unrelated nftables policy, and attaching directly to the delegated application cgroup. It still needs a privileged loader, pinned-program lifecycle, kernel compatibility checks, UDP/443 denial, and equivalent fail-closed tests. The nftables prototype remains useful as a simple baseline and fallback; eBPF must not introduce different routing or policy semantics.