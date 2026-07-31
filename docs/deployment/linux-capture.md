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
cargo run --bin agentgateway-edge-capture -- \
  --listen 127.0.0.1:15001 \
  --hbone-endpoint 127.0.0.1:15008
```

Both endpoints are restricted to loopback. The relay uses Linux `SO_ORIGINAL_DST`, preserves raw bytes, bounds concurrent tunnels, and closes failed or overloaded flows without direct fallback. Its current HBONE connection is cleartext, unauthenticated, and established once at startup. This is suitable only for isolated local interoperability work; trusted standalone or managed use requires an authenticated Agent Gateway contract and reconnect lifecycle.

## Isolated validation

Run the opt-in behavior test with Podman:

```bash
sh tests/linux-capture-container.sh
```

The test uses rootless Podman with private network and cgroup namespaces. Rootless `--privileged` grants the disposable container enough namespaced privilege to create a child cgroup and nftables table; it does not use host network or cgroup namespaces. It builds and runs the real relay against a minimal packaged Python h2 peer, then proves original-destination CONNECT authority, bidirectional bytes, TCP/443 redirection, UDP/443 rejection, counters, and scoped table removal. It downloads Rust dependencies, nftables, and small networking tools into the disposable container.

This does not prove production host attribution, resistance to a local administrator, compatibility with an existing host firewall, or interoperability with an authenticated real Agent Gateway. Those require a disposable host or VM and the agreed Agent Gateway authentication boundary.

## eBPF strengthening path

An eBPF cgroup `connect4`/`connect6` implementation can improve the production design by selecting sockets at connect time, preserving destination metadata without conntrack lookup, reducing interaction with unrelated nftables policy, and attaching directly to the delegated application cgroup. It still needs a privileged loader, pinned-program lifecycle, kernel compatibility checks, UDP/443 denial, and equivalent fail-closed tests. The nftables prototype remains useful as a simple baseline and fallback; eBPF must not introduce different routing or policy semantics.