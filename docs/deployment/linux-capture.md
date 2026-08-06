# Linux transparent capture

The supported standalone Linux path uses an Agent Desktop-owned systemd user scope and nftables. The connector's unprivileged in-process relay recovers the original destination and opens one authenticated HTTP/2 CONNECT stream per redirected TCP flow.

## Behavior

The privileged setup binary owns one `inet agentdesktop` table and one named `captured_cgroups` set. Two stable rules look up the socket's cgroup in that set at the systemd scope hierarchy depth. Each set member selects one scope and its descendant cgroups, so child and helper processes remain in the application profile without executable-name matching. Concurrent captured applications share the rules and have independent set members.

For matching sockets, the table:

- Redirects TCP destination port 443 to the configured local capture listener.
- Rejects UDP destination port 443 so QUIC cannot bypass inspection.
- Counts both actions without logging destinations, processes, or payloads.
- Leaves all other traffic and all other cgroups unchanged.

Rules and the root-owned `/run/agentdesktop/capture-state.json` registry are ephemeral and do not survive reboot. The helper serializes mutations with `/run/agentdesktop/capture.lock`, removes registry entries whose cgroup directories no longer exist, and submits one atomic nftables transaction that flushes and repopulates the set from all remaining live paths. Packets therefore see either the complete old set or the complete new set. A rejected transaction preserves both the previous kernel set and committed registry.

The registry exists because nftables accepts cgroup paths as input but cannot parse its printed numeric cgroup IDs after a cgroup directory disappears. Agent Desktop never sends numeric cgroup elements through the `nft` parser. Normal removal unregisters the requested path; stale entries are reconciled on the next helper operation. All registered scopes must use the same hierarchy depth and redirect port.

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
sudo agentdesktop-capture-setup remove \
  --cgroup /user.slice/user-1000.slice/app.slice/claude.scope
```

Do not place the connector or Agent Gateway in the selected application scope. Doing so can redirect the tunnel transport back into the capture listener.

Start the diagnostic relay outside the selected scope after an HBONE listener is ready:

```bash
cargo run --bin agentdesktop -- capture \
  --listen 127.0.0.1:15001 \
  --hbone-endpoint 127.0.0.1:15008 \
  --token-file "$XDG_RUNTIME_DIR/agentdesktop/capture-token"
```

The `capture` subcommand is a diagnostic boundary rather than a separate product binary. Its token file is required, must be a regular file owned by the current user with mode `0600`, and must contain a non-empty HTTP header value. Supported standalone operation does not use this file: Agent Desktop generates one token in memory for each connector-owned Agent Gateway process, injects it through the Gateway startup environment, and retains the same value for the in-process relay. Agent Gateway authorizes the re-entered route with `source.connectHeaders["x-agentdesktop-token"]`. The smoke config demonstrates this policy without creating a second policy format.

Both endpoints are restricted to loopback. The relay uses Linux `SO_ORIGINAL_DST`, preserves raw bytes, bounds concurrent tunnels, and closes failed or overloaded flows without direct fallback. Its cleartext HBONE pool reconnects lazily for later flows after observed transport loss; it never retries a failed CONNECT or replays inner bytes. Relay startup has an in-process readiness boundary that completes only after endpoint validation, token loading, HBONE handshake, and listener bind. The 256-bit in-memory token rotates with the connector-owned Gateway process rather than with each application session, so concurrent capture sessions do not require Gateway restarts or token files. Managed capture will instead require mTLS with immutable certificate-derived user/device context.

## Isolated validation

Run the opt-in behavior test with Podman:

```bash
sh tests/linux-capture-container.sh
```

The test uses rootless Podman with private network and cgroup namespaces. Rootless `--privileged` grants the disposable container enough namespaced privilege to create child cgroups and an nftables table; it does not use host network or cgroup namespaces. It builds and runs the real relay against a minimal packaged Python h2 peer, then proves original-destination CONNECT authority, bidirectional bytes, TCP/443 redirection, UDP/443 rejection, counters, two concurrent set members, independent removal, and stale-cgroup reconciliation. It downloads Rust dependencies, nftables, and small networking tools into the disposable container.

This does not prove production host attribution, resistance to a local administrator, or compatibility with an existing host firewall. Those require a disposable host or VM.

## Application execution scope

The public launch boundary can already place a command and all descendants in an owned transient systemd user scope:

```bash
agentdesktop launch --profile custom -- command --args
```

The scope starts an internal gated child, validates the exact systemd `ControlGroup` against the cgroup v2 filesystem, and releases the child only afterward. A random Linux abstract Unix socket carries the readiness and release handshake; the child exits without launching the application if the controller disconnects before release. Readiness and gate writes time out after two seconds, while the child allows up to five minutes for capture setup and interactive authorization before release. The default `custom` profile supplies no integration environment.

The `claude` profile is reserved for transparent capture, injects no connector-routing environment, and rejects inherited or persistent `ANTHROPIC_BASE_URL` routing before creating a scope. It verifies the exact installed inspection root and ready relay, registers the scope while the child remains gated, releases the application only after success, and unregisters it after the complete scope exits. Claude configured with the native or connector-assisted path runs normally without `agentdesktop launch`; enabling both paths would risk duplicate routing or loops.

Preparation failure terminates and reaps the scope without executing the target. Gateway or relay failure closes redirected connections without direct fallback while rules remain active until the scope exits. Scope emptiness and controller state, not Claude hooks or individual PIDs, determine cleanup.

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

An eBPF cgroup `connect4`/`connect6` implementation can improve the production design by selecting sockets at connect time, preserving destination metadata without conntrack lookup, reducing interaction with unrelated nftables policy, and attaching directly to the delegated application cgroup. It still needs a privileged loader, pinned-program lifecycle, kernel compatibility checks, UDP/443 denial, and equivalent fail-closed tests. The nftables implementation remains the supported Linux baseline; eBPF must not introduce different routing or policy semantics.