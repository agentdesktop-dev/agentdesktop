# Linux capture prototype

The first Linux transparent-capture mechanism targets an externally managed cgroup v2 scope and nftables. A separate unprivileged relay recovers the original destination and opens one HTTP/2 CONNECT stream per redirected TCP flow. It is a prototype boundary and does not enable the `captured` application path yet.

## Behavior

The privileged setup binary owns one `inet agentgateway_edge` table. For every component in the configured absolute cgroup path, it emits a `socket cgroupv2 level` match. This selects the scope and descendant cgroups, so child and helper processes remain in the application profile without executable-name matching.

For matching sockets, the table:

- Redirects TCP destination port 443 to the configured local capture listener.
- Rejects UDP destination port 443 so QUIC cannot bypass inspection.
- Counts both actions without logging destinations, processes, or payloads.
- Leaves all other traffic and all other cgroups unchanged.

Rules are ephemeral and do not survive reboot. Installation replaces the connector-owned table in one nftables transaction. Removal deletes only that table and is a no-op when it is absent.

## Setup boundary

The application scope must already exist. Render rules without privilege:

```bash
cargo run --bin agentgateway-edge-capture-setup -- render \
  --cgroup /user.slice/user-1000.slice/app.slice/claude.scope \
  --redirect-port 15001
```

Preflight, installation, and removal require root because nftables validation uses kernel netlink:

```bash
sudo agentgateway-edge-capture-setup preflight \
  --cgroup /user.slice/user-1000.slice/app.slice/claude.scope \
  --redirect-port 15001
sudo agentgateway-edge-capture-setup install \
  --cgroup /user.slice/user-1000.slice/app.slice/claude.scope \
  --redirect-port 15001
sudo agentgateway-edge-capture-setup remove
```

Do not place the connector or Agent Gateway in the selected application scope. Doing so can redirect the tunnel transport back into the capture listener.

Start the prototype relay outside the selected scope after an HBONE listener is ready:

```bash
cargo run --bin agentgateway-edge-connector -- capture \
  --listen 127.0.0.1:15001 \
  --hbone-endpoint 127.0.0.1:15008 \
  --token-file "$XDG_RUNTIME_DIR/agentgateway-edge/capture-token"
```

The capture relay is a connector subcommand rather than a separate product binary. The token file is required, must be a regular file owned by the current user with mode `0600`, and must contain a non-empty HTTP header value. Agent Gateway must receive the same value through its protected startup environment and authorize the re-entered route with `source.connectHeaders["x-agentgateway-edge-token"]`. The smoke config demonstrates this policy without creating a second policy format.

Both endpoints are restricted to loopback. The relay uses Linux `SO_ORIGINAL_DST`, preserves raw bytes, bounds concurrent tunnels, and closes failed or overloaded flows without direct fallback. Its cleartext HBONE pool reconnects lazily for later flows after observed transport loss; it never retries a failed CONNECT or replays inner bytes. The opaque token prevents unrelated local clients from using a correctly configured tunnel listener, but creation, rotation, and process lifecycle are not integrated. Managed use additionally requires TLS and DPoP-bound organizational identity.

## Isolated validation

Run the opt-in behavior test with Podman:

```bash
sh tests/linux-capture-container.sh
```

The test uses rootless Podman with private network and cgroup namespaces. Rootless `--privileged` grants the disposable container enough namespaced privilege to create a child cgroup and nftables table; it does not use host network or cgroup namespaces. It builds and runs the real relay against a minimal packaged Python h2 peer, then proves original-destination CONNECT authority, bidirectional bytes, TCP/443 redirection, UDP/443 rejection, counters, and scoped table removal. It downloads Rust dependencies, nftables, and small networking tools into the disposable container.

This does not prove production host attribution, resistance to a local administrator, or compatibility with an existing host firewall. Those require a disposable host or VM.

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